use {
    super::{
        etcd_utils::lock::ManagedLock,
        prom::{
            scylladb_batch_request_lag_inc, scylladb_batch_request_lag_sub,
            scylladb_batch_sent_inc, scylladb_batch_size_observe, scylladb_batchitem_sent_inc_by,
        },
        scylladb_utils::LwtResult,
        types::{
            AccountUpdate, BlockchainEvent, CommitmentLevel, ProducerId, ProducerInfo, ShardId,
            ShardOffset, ShardPeriod, Slot, Transaction, SHARD_OFFSET_MODULO, UNDEFINED_SLOT,
        },
    },
    crate::scylladb::{
        etcd_utils, yellowstone_log::consumer_group::etcd_path::get_producer_lock_path_v1,
    },
    deepsize::DeepSizeOf,
    futures::{
        future::{self, try_join_all},
        Future,
    },
    local_ip_address::{list_afinet_netifas, local_ip},
    scylla::{
        batch::{Batch, BatchType},
        frame::Compression,
        Session, SessionBuilder,
    },
    std::{
        collections::{BTreeMap, BTreeSet},
        net::IpAddr,
        sync::Arc,
        time::Duration,
    },
    tokio::{
        sync::mpsc::{error::SendError, Permit},
        task::{JoinError, JoinHandle},
        time::Instant,
    },
    tracing::{error, info, warn},
    uuid::Uuid,
};

const WARNING_SCYLLADB_LATENCY_THRESHOLD: Duration = Duration::from_millis(1000);

const DEFAULT_SHARD_MAX_BUFFER_CAPACITY: usize = 15;

/// Untyped API in scylla will soon be deprecated, this is why we need to implement our own deser logic to
/// only read the first column returned by a light weight transaction.

const INSERT_PRODUCER_SLOT: &str = r###"
    INSERT INTO producer_slot_seen (producer_id, slot, shard_offset_map, created_at)
    VALUES (?, ?, ?, currentTimestamp())
"###;

const INSERT_INITIAL_PRODUCER_LOCK_STATE: &str = r###"
    INSERT INTO producer_lock (producer_id, ifname, ipv4, is_ready, minimum_shard_offset, created_at)
    VALUES (?, ?, ?, false, null, currentTimestamp())
    IF NOT EXISTS
"###;

const INSERT_PRODUCER_INFO_LEGACY: &str = r###"
    INSERT INTO producer_info (producer_id, commitment_level, num_shards, created_at, updated_at)
    VALUES (?, ?, ?, currentTimestamp(), currentTimestamp())
    IF NOT EXISTS
"###;

const COMMIT_SHARD_PERIOD: &str = r###"
    INSERT INTO producer_period_commit_log (producer_id, shard_id, period, created_at)
    VALUES (?, ?, ?, currentTimestamp())
"###;

const INSERT_BLOCKCHAIN_EVENT: &str = r###"
    INSERT INTO log (
        shard_id, 
        period,
        producer_id,
        offset,
        slot,
        event_type,
        pubkey, 
        lamports, 
        owner, 
        executable, 
        rent_epoch, 
        write_version, 
        data, 
        txn_signature,
        signature,
        signatures,
        num_readonly_signed_accounts, 
        num_readonly_unsigned_accounts,
        num_required_signatures,
        account_keys, 
        recent_blockhash, 
        instructions, 
        versioned,
        address_table_lookups, 
        meta,
        is_vote,
        tx_index,
        created_at
    )
    VALUES (?,?,?, ?,?,?,  ?,?,?, ?,?,?, ?,?,?, ?,?,?, ?,?,?, ?,?,?, ?,?,?, currentTimestamp())
"###;

#[derive(Clone, PartialEq, Debug)]
pub struct ScyllaSinkConfig {
    pub producer_id: Option<Uuid>,
    pub num_shards: usize,
    pub batch_len_limit: usize,
    pub batch_size_kb_limit: usize,
    pub linger: Duration,
    pub keyspace: String,
    pub ifname: Option<String>,
    pub commitment_level: CommitmentLevel,
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, PartialEq)]
enum ShardCommand {
    Shutdown,
    // Add other action if necessary...
    InsertAccountUpdate(AccountUpdate),
    InsertTransaction(Transaction),
}

/// Represents a shard responsible for processing and batching `ShardCommand` messages
/// before committing them to the database in a background daemon.
///
/// This struct encapsulates the state and behavior required to manage message buffering,
/// batching, and period-based commitment for a specific shard within a distributed system.
struct Shard {
    /// Arc-wrapped database session for executing queries.
    session: Arc<Session>,

    /// Unique identifier for the shard.
    shard_id: ShardId,

    /// Unique identifier for the producer associated with this shard.
    producer_id: ProducerId,

    /// The next offset to be assigned for incoming client commands.
    next_offset: ShardOffset,

    /// Buffer to store sharded client commands before batching.
    buffer: Vec<BlockchainEvent>,

    /// Maximum capacity of the buffer (number of commands it can hold).
    max_buffer_capacity: usize,

    /// Maximum byte size of the buffer (sum of sizes of commands it can hold).
    max_buffer_byte_size: usize,

    /// Batch for executing database statements in bulk.
    scylla_batch: Batch,

    /// Current byte size of the batch being constructed.
    curr_batch_byte_size: usize,

    /// Duration to linger before flushing the buffer.
    buffer_linger: Duration,

    last_committed_period: ShardPeriod,
}

impl Shard {
    fn new(
        session: Arc<Session>,
        shard_id: ShardId,
        producer_id: ProducerId,
        next_offset: ShardOffset,
        max_buffer_capacity: usize,
        max_buffer_byte_size: usize,
        buffer_linger: Duration,
    ) -> Self {
        if next_offset < 0 {
            panic!("next offset can not be negative");
        }
        Shard {
            session,
            shard_id,
            producer_id,
            next_offset,
            buffer: Vec::with_capacity(max_buffer_capacity),
            max_buffer_capacity,
            max_buffer_byte_size,
            // Since each shard will only batch into a single partition at a time, we can safely disable batch logging
            // without losing atomicity guarantee provided by scylla.
            scylla_batch: Batch::new(BatchType::Unlogged),
            buffer_linger,
            curr_batch_byte_size: 0,
            last_committed_period: -1,
        }
    }

    fn clear_buffer(&mut self) {
        self.buffer.clear();
        self.curr_batch_byte_size = 0;
        self.scylla_batch.statements.clear();
    }

    async fn flush(&mut self) -> anyhow::Result<()> {
        let buffer_len = self.buffer.len();
        if buffer_len > 0 {
            let before = Instant::now();
            // We must wait for the batch success to guarantee monotonicity in the shard's timeline.
            self.session.batch(&self.scylla_batch, &self.buffer).await?;
            scylladb_batch_request_lag_sub(buffer_len as i64);
            scylladb_batch_sent_inc();
            scylladb_batch_size_observe(buffer_len);
            scylladb_batchitem_sent_inc_by(buffer_len as u64);
            if before.elapsed() >= WARNING_SCYLLADB_LATENCY_THRESHOLD {
                warn!("sent {} elements in {:?}", buffer_len, before.elapsed());
            }
        }
        self.clear_buffer();
        Ok(())
    }

    /// Converts the current `Shard` instance into a background daemon for processing and batching `ShardCommand` messages.
    ///
    /// This method spawns an asynchronous task (`tokio::spawn`) to continuously receive messages from a channel (`receiver`),
    /// batch process them, and commit periods to the database. It handles message buffering
    /// and period commitment based on the configured buffer settings and period boundaries.
    ///
    /// # Returns
    /// Returns a `Sender` channel (`tokio::sync::mpsc::Sender<ShardCommand>`) that can be used to send `ShardCommand` messages
    /// to the background daemon for processing and batching.
    fn into_daemon(mut self) -> ShardHandle {
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<ShardCommand>(16);
        let shard_id = self.shard_id;
        let (wsender, wreceiver) = tokio::sync::watch::channel(self.next_offset - 1);

        let handle: JoinHandle<anyhow::Result<()>> = tokio::spawn(async move {
            let shard_id = self.shard_id;
            let producer_id = self.producer_id;
            let insert_event_ps = self.session.prepare(INSERT_BLOCKCHAIN_EVENT).await?;
            let commit_period_ps = self.session.prepare(COMMIT_SHARD_PERIOD).await?;
            let mut buffering_timeout = Instant::now() + self.buffer_linger;
            loop {
                let offset = self.next_offset;
                let curr_period = offset / SHARD_OFFSET_MODULO;
                let prev_period = curr_period - 1;

                // If we started a new period
                if offset % SHARD_OFFSET_MODULO == 0
                    && offset > 0
                    && self.last_committed_period != prev_period
                {
                    // Make sure the last period is committed
                    let t = Instant::now();
                    self.session
                        .execute(&commit_period_ps, (producer_id, shard_id, prev_period))
                        .await?;
                    info!(
                        shard = shard_id,
                        producer_id = ?self.producer_id,
                        committed_period = curr_period,
                        time_to_commit = ?t.elapsed()
                    );
                    self.last_committed_period = prev_period;
                }

                let msg = receiver
                    .recv()
                    .await
                    .ok_or(anyhow::anyhow!("Shard mailbox closed"))?;

                let maybe_blockchain_event = match msg {
                    ShardCommand::Shutdown => {
                        warn!("Shard {} received shutdown command.", shard_id);
                        self.flush().await?;
                        warn!("shard {} finished shutdown procedure", shard_id);
                        return Ok(());
                    }
                    ShardCommand::InsertAccountUpdate(acc_update) => {
                        Some(acc_update.as_blockchain_event(shard_id, producer_id, offset))
                    }
                    ShardCommand::InsertTransaction(new_tx) => {
                        Some(new_tx.as_blockchain_event(shard_id, producer_id, offset))
                    }
                };

                if let Some(blockchain_event) = maybe_blockchain_event {
                    let msg_byte_size = blockchain_event.deep_size_of();

                    let need_flush = self.buffer.len() >= self.max_buffer_capacity
                        || self.curr_batch_byte_size + msg_byte_size >= self.max_buffer_byte_size
                        || buffering_timeout.elapsed() > Duration::ZERO;

                    if need_flush {
                        self.flush().await?;
                        buffering_timeout = Instant::now() + self.buffer_linger;
                    }

                    self.buffer.push(blockchain_event);
                    self.scylla_batch.append_statement(insert_event_ps.clone());
                    self.curr_batch_byte_size += msg_byte_size;
                    wsender
                        .send(offset)
                        .map_err(|_offset| anyhow::anyhow!("failed to notify committed offset"))?;
                    self.next_offset += 1;
                }
            }
        });

        ShardHandle {
            shard_id,
            sender,
            tokio_handle: handle,
            shard_offset_watch: wreceiver,
        }
    }
}

struct ShardHandle {
    shard_id: ShardId,
    sender: tokio::sync::mpsc::Sender<ShardCommand>,
    tokio_handle: JoinHandle<anyhow::Result<()>>,
    shard_offset_watch: tokio::sync::watch::Receiver<ShardOffset>,
}

impl ShardHandle {
    async fn reserve(&self) -> Result<Permit<'_, ShardCommand>, SendError<()>> {
        self.sender.reserve().await
    }

    async fn send(&self, value: ShardCommand) -> Result<(), SendError<ShardCommand>> {
        self.sender.send(value).await
    }

    fn get_last_committed_offset(&self) -> ShardOffset {
        self.shard_offset_watch.borrow().to_owned()
    }
}

impl Future for ShardHandle {
    type Output = Result<anyhow::Result<()>, JoinError>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let handle = &mut self.tokio_handle;
        tokio::pin!(handle);
        handle.poll(cx)
    }
}

pub struct ScyllaSink {
    router_sender: tokio::sync::mpsc::Sender<ShardCommand>,
    router_handle: JoinHandle<anyhow::Result<()>>,
    producer_lock: ProducerLock,
}

#[derive(Debug)]
pub enum ScyllaSinkError {
    SinkClose,
}

/// Retrieves the latest shard offsets for a specific producer from the `shard_max_offset_mv` materialized view.
///
/// This asynchronous function queries the database session to fetch the latest shard offsets associated with
/// a given `producer_id` from the `shard_max_offset_mv` materialized view. It constructs and executes a SELECT
/// query to retrieve the shard IDs and corresponding offsets ordered by offset and period.
///
/// # Parameters
/// - `session`: An Arc-wrapped database session (`Arc<Session>`) for executing database queries.
/// - `producer_id`: The unique identifier (`ProducerId`) of the producer whose shard offsets are being retrieved.
/// - `num_shards` : number of shard assigned to producer.
///
/// # Returns
/// - `Ok(None)`: If no shard offsets are found for the specified producer.
/// - `Ok(Some(rows))`: If shard offsets are found, returns a vector of tuples containing shard IDs and offsets.
///                      Each tuple represents a shard's latest offset for the producer.
/// - `Err`: If an error occurs during database query execution or result parsing, returns an `anyhow::Result`.
pub(crate) async fn get_max_shard_offsets_for_producer(
    session: Arc<Session>,
    producer_id: ProducerId,
    num_shards: usize,
) -> anyhow::Result<BTreeMap<ShardId, (ShardOffset, Slot)>> {
    let shard_list = (0..num_shards)
        .map(|idx| idx as ShardId)
        .collect::<Vec<_>>();

    let query_last_period_commit = r###"
        SELECT
            shard_id,
            period
        FROM producer_period_commit_log
        where producer_id = ?
        AND shard_id IN ?
        ORDER BY period DESC
        PER PARTITION LIMIT 1
    "###;

    let mut current_period_foreach_shard = session
        .query(query_last_period_commit, (producer_id, shard_list))
        .await?
        .rows_typed_or_empty::<(ShardId, ShardPeriod)>()
        .map(|result| result.map(|(shard_id, period)| (shard_id, period + 1)))
        .collect::<Result<BTreeMap<_, _>, _>>()?;

    for shard_id in 0..num_shards {
        // Put period 0 by default for each missing shard.
        current_period_foreach_shard
            .entry(shard_id as ShardId)
            .or_insert(0);
    }

    let query_max_offset_for_shard_period = r###"
        SELECT
            offset,
            slot
        FROM log
        WHERE 
            producer_id = ?
            AND shard_id = ?
            and period = ?
        ORDER BY offset desc
        PER PARTITION LIMIT 1        
    "###;
    let max_offset_for_shard_period_ps = session.prepare(query_max_offset_for_shard_period).await?;

    //let mut js: JoinSet<anyhow::Result<(i16, i64)>> = JoinSet::new();
    let shard_max_offset_pairs =
        futures::future::try_join_all(current_period_foreach_shard.iter().map(
            |(shard_id, curr_period)| {
                let ps = max_offset_for_shard_period_ps.clone();
                let session = Arc::clone(&session);
                async move {
                    let (max_offset, slot) = session
                        .execute(&ps, (producer_id, shard_id, curr_period))
                        .await?
                        .maybe_first_row_typed::<(ShardOffset, Slot)>()?
                        // If row is None, it means no period has started since the last period commit.
                        // So we seek at the end of the previous period.
                        .unwrap_or(((curr_period * SHARD_OFFSET_MODULO) - 1, UNDEFINED_SLOT));
                    Ok::<_, anyhow::Error>((*shard_id, max_offset, slot))
                }
            },
        ))
        .await?;

    if shard_max_offset_pairs.len() != num_shards {
        panic!("missing shard period commit information, make sure the period commit is initialize before computing shard offsets");
    }

    let ret = shard_max_offset_pairs
        .into_iter()
        .map(|(a, b, c)| (a, (b, c)))
        .collect();

    Ok(ret)
}

pub(crate) async fn get_max_shard_offsets_for_producer_v2(
    session: Arc<Session>,
    producer_id: ProducerId,
    num_shards: usize,
) -> anyhow::Result<BTreeMap<ShardId, ShardOffset>> {
    let shard_list = (0..num_shards)
        .map(|idx| idx as ShardId)
        .collect::<Vec<_>>();

    let query_last_period_commit = r###"
        SELECT
            shard_id,
            period
        FROM producer_period_commit_log
        where producer_id = ?
        AND shard_id IN ?
        ORDER BY period DESC
        PER PARTITION LIMIT 1
    "###;

    let mut current_period_foreach_shard = session
        .query(query_last_period_commit, (producer_id, shard_list))
        .await?
        .rows_typed_or_empty::<(ShardId, ShardPeriod)>()
        .map(|result| result.map(|(shard_id, period)| (shard_id, period + 1)))
        .collect::<Result<BTreeMap<_, _>, _>>()?;

    for shard_id in 0..num_shards {
        // Put period 0 by default for each missing shard.
        current_period_foreach_shard
            .entry(shard_id as ShardId)
            .or_insert(0);
    }

    let query_max_offset_for_shard_period = r###"
        SELECT
            offset
        FROM log
        WHERE 
            shard_id = ?
            AND producer_id = ?
            AND period = ?
        ORDER BY offset desc
        PER PARTITION LIMIT 1        
    "###;
    let max_offset_for_shard_period_ps = session.prepare(query_max_offset_for_shard_period).await?;

    //let mut js: JoinSet<anyhow::Result<(i16, i64)>> = JoinSet::new();
    let shard_max_offset_pairs =
        futures::future::try_join_all(current_period_foreach_shard.iter().map(
            |(shard_id, curr_period)| {
                let ps = max_offset_for_shard_period_ps.clone();
                let session = Arc::clone(&session);
                async move {
                    let max_offset = session
                        .execute(&ps, (shard_id, producer_id, curr_period))
                        .await?
                        .maybe_first_row_typed::<(ShardOffset,)>()?
                        .map(|row| row.0)
                        .unwrap_or(0);
                    Ok::<_, anyhow::Error>((*shard_id, max_offset))
                }
            },
        ))
        .await?;

    if shard_max_offset_pairs.len() != num_shards {
        panic!("missing shard period commit information, make sure the period commit is initialize before computing shard offsets");
    }

    let ret = shard_max_offset_pairs
        .into_iter()
        .map(|(a, b)| (a, b))
        .collect();

    Ok(ret)
}

/// Spawns a round-robin dispatcher for sending `ShardCommand` messages to a list of shard mailboxes.
///
/// This function takes a vector of shard mailboxes (`tokio::sync::mpsc::Sender<ShardCommand>`) and returns
/// a new `Sender` that can be used to dispatch messages in a round-robin fashion to the provided shard mailboxes.
///
/// The dispatcher cycles through the shard mailboxes indefinitely, ensuring each message is sent to the next
/// available shard without waiting, or falling back to the original shard if all are busy. It increments the
/// ScyllaDB batch request lag for monitoring purposes.
///
/// # Parameters
/// - `shard_mailboxes`: A vector of `Sender` channels representing shard mailboxes to dispatch messages to.
///
/// # Returns
/// A `Sender` channel that can be used to send `ShardCommand` messages to the shard mailboxes in a round-robin manner.
fn spawn_round_robin(
    session: Arc<Session>,
    producer_id: ProducerId,
    shard_handles: Vec<ShardHandle>,
    managed_lock: Arc<ManagedLock>,
) -> (
    tokio::sync::mpsc::Sender<ShardCommand>,
    JoinHandle<anyhow::Result<()>>,
) {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(DEFAULT_SHARD_MAX_BUFFER_CAPACITY);

    let h: JoinHandle<anyhow::Result<()>> = tokio::spawn(async move {
        let insert_slot_ps = session.prepare(INSERT_PRODUCER_SLOT).await?;
        // One hour worth of slots
        const SLOT_SEEN_RETENTION: usize = 9000;

        let iterator = shard_handles.iter().enumerate().cycle();
        info!("Started round robin router");
        let mut msg_between_slot = 0;
        let mut max_slot_seen = -1;
        let mut time_since_new_max_slot = Instant::now();
        let mut background_commit_slot_seen =
            tokio::spawn(future::ready(Ok::<(), anyhow::Error>(())));

        let mut slots_seen = BTreeSet::<Slot>::new();

        for (i, shard_sender) in iterator {
            let msg = receiver.recv().await.unwrap_or(ShardCommand::Shutdown);

            if msg == ShardCommand::Shutdown {
                warn!("round robin router's mailbox closed unexpectly.");
                break;
            }
            let slot = match &msg {
                ShardCommand::Shutdown => -1,
                ShardCommand::InsertAccountUpdate(x) => x.slot,
                ShardCommand::InsertTransaction(x) => x.slot,
            };

            if slots_seen.insert(slot) {
                while slots_seen.len() >= SLOT_SEEN_RETENTION {
                    slots_seen.pop_first();
                }

                if max_slot_seen > slot {
                    warn!("Slot {slot} arrived late after seeing {max_slot_seen}");
                } else {
                    max_slot_seen = slot;
                }
                let time_elapsed_between_last_max_slot = time_since_new_max_slot.elapsed();
                // We only commit every 3 slot number

                let t = Instant::now();
                background_commit_slot_seen.await??;

                let session = Arc::clone(&session);
                let insert_slot_ps = insert_slot_ps.clone();
                //let update_producer_lock_ps = update_producer_lock_ps.clone();
                let shard_offset_pairs = shard_handles
                    .iter()
                    .map(|sh| (sh.shard_id, sh.get_last_committed_offset()))
                    .collect::<Vec<_>>();

                let managed_lock = Arc::clone(&managed_lock);
                background_commit_slot_seen = tokio::spawn(async move {
                    // Asking a fencing token will fail if the lock is revoked.

                    session
                        .execute(&insert_slot_ps, (producer_id, slot, shard_offset_pairs))
                        .await?;

                    let time_to_commit_slot = t.elapsed();
                    info!(
                        "New slot: {} after {time_elapsed_between_last_max_slot:?}, events in between: {}, max_slot_approx committed in {time_to_commit_slot:?}",
                        slot, msg_between_slot
                    );
                    Ok(())
                });
                time_since_new_max_slot = Instant::now();
                msg_between_slot = 0;
            }
            msg_between_slot += 1;
            let result = shard_sender.reserve().await;
            if let Ok(permit) = result {
                permit.send(msg);
                scylladb_batch_request_lag_inc();
            } else {
                error!("shard {} seems to be closed: {:?}", i, result);
                break;
            }
        }
        // Send shutdown to all shards
        for (i, shard_sender) in shard_handles.iter().enumerate() {
            warn!("Shutting down shard: {}", i);
            shard_sender.send(ShardCommand::Shutdown).await?;
        }

        try_join_all(shard_handles.into_iter()).await?;

        warn!("End of round robin router");
        Ok(())
    });
    (sender, h)
}

async fn insert_producer_info_legacy(
    session: Arc<Session>,
    producer_id: ProducerId,
    commitment_level: CommitmentLevel,
    num_shards: usize,
) -> anyhow::Result<()> {
    let lwt_result = session
        .query(
            INSERT_PRODUCER_INFO_LEGACY,
            (producer_id, commitment_level, num_shards as i16),
        )
        .await?
        .first_row_typed::<LwtResult>()?;

    anyhow::ensure!(
        lwt_result.succeeded(),
        "Producer info already exists in scylladb"
    );
    Ok(())
}

struct ProducerLock {
    session: Arc<Session>,
    producer_id: ProducerId,
}

async fn load_producer_lock_state(
    session: Arc<Session>,
    producer_id: ProducerId,
    ifname: Option<String>,
) -> anyhow::Result<ProducerLock> {
    let network_interfaces = list_afinet_netifas()?;

    let (ifname, ipaddr) = if let Some(ifname) = ifname {
        if let Some((_, ipaddr)) = network_interfaces
            .iter()
            .find(|(name, ipaddr)| *name == ifname && matches!(ipaddr, IpAddr::V4(_)))
        {
            (ifname, *ipaddr)
        } else {
            anyhow::bail!("Found not interface named {}", ifname);
        }
    } else {
        let ipaddr = local_ip()?;
        if !ipaddr.is_ipv4() {
            anyhow::bail!("ipv6 not support for producer lock info.");
        }
        if let Some((ifname, _)) = network_interfaces
            .iter()
            .find(|(_, ipaddr2)| ipaddr == *ipaddr2)
        {
            (ifname.to_owned(), ipaddr)
        } else {
            anyhow::bail!("Found not interface matching ip {}", ipaddr);
        }
    };

    let lwt_result = session
        .query(
            INSERT_INITIAL_PRODUCER_LOCK_STATE,
            (producer_id, ifname, ipaddr),
        )
        .await?
        .first_row_typed::<LwtResult>()?;
    anyhow::ensure!(
        lwt_result.succeeded(),
        "Producer lock state already exists in scylladb"
    );
    let lock = ProducerLock {
        session: Arc::clone(&session),
        producer_id,
    };
    Ok(lock)
}

async fn set_minimum_producer_offsets(
    session: Arc<Session>,
    producer_lock: &ProducerLock,
    minimum_shard_offsets: &BTreeMap<ShardId, ShardOffset>,
    initial_slot: Slot,
) -> anyhow::Result<()> {
    let minimum_shard_offsets = minimum_shard_offsets
        .iter()
        .map(|(k, v)| (*k, (*v, initial_slot)))
        .collect::<BTreeMap<_, _>>();

    let ps = session
        .prepare(
            r###"
        UPDATE producer_lock
        SET minimum_shard_offset = ?, is_ready = true
        WHERE 
            producer_id = ?
        IF EXISTS
        "###,
        )
        .await?;
    info!("before setting minimum shard offsets...");
    let lwt = session
        .execute(&ps, (minimum_shard_offsets, producer_lock.producer_id))
        .await?
        .first_row_typed::<LwtResult>()?;

    anyhow::ensure!(
        lwt.succeeded(),
        "failed to set producer ready state in scylladb, producer lock state has been revoked."
    );
    Ok(())
}

impl ScyllaSink {
    pub async fn new(
        etcd: etcd_client::Client,
        config: ScyllaSinkConfig,
        initital_slot: Slot,
        hostname: impl AsRef<str>,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> anyhow::Result<Self> {
        let producer_id = ProducerId::from(config.producer_id.unwrap_or(Uuid::new_v4()));
        let session: Session = SessionBuilder::new()
            .known_node(hostname)
            .user(username, password)
            .compression(Some(Compression::Lz4))
            .use_keyspace(config.keyspace.clone(), false)
            .build()
            .await?;
        info!("connection pool to scylladb ready.");
        let session = Arc::new(session);

        insert_producer_info_legacy(
            Arc::clone(&session),
            producer_id,
            config.commitment_level,
            config.num_shards,
        )
        .await?;

        info!("Producer {producer_id:?} is registered");

        let etcd_lock_path = get_producer_lock_path_v1(producer_id);
        let managed_lock = etcd_utils::lock::try_lock(etcd.clone(), &etcd_lock_path).await?;
        let managed_lock = Arc::new(managed_lock);
        let producer_lock =
            load_producer_lock_state(Arc::clone(&session), producer_id, config.ifname.to_owned())
                .await?;

        info!("Producer {producer_id:?} lock acquired!");

        let shard_count = config.num_shards;

        // On init, we collect where the producer left = max shard offsets
        // Where we left of, it becomes new earliest offset available.
        // This is to prevent
        let shard_offsets =
            get_max_shard_offsets_for_producer_v2(Arc::clone(&session), producer_id, shard_count)
                .await?;

        set_minimum_producer_offsets(
            Arc::clone(&session),
            &producer_lock,
            &shard_offsets,
            initital_slot,
        )
        .await?;

        info!("Got back last offsets of all {shard_count} shards");
        let mut shard_handles = Vec::with_capacity(shard_count);
        for (shard_id, last_offset) in shard_offsets.into_iter() {
            let session = Arc::clone(&session);
            let shard = Shard::new(
                session,
                shard_id,
                producer_id,
                last_offset + 1,
                DEFAULT_SHARD_MAX_BUFFER_CAPACITY,
                config.batch_size_kb_limit * 1024,
                config.linger,
            );
            let shard_handle = shard.into_daemon();
            shard_handles.push(shard_handle);
        }

        let (sender, router_handle) = spawn_round_robin(
            Arc::clone(&session),
            producer_id,
            shard_handles,
            Arc::clone(&managed_lock),
        );

        Ok(ScyllaSink {
            router_sender: sender,
            router_handle,
            producer_lock,
        })
    }

    pub async fn shutdown(self) -> anyhow::Result<()> {
        warn!("Shutthing down scylla sink...");
        let router_result = self.router_sender.send(ShardCommand::Shutdown).await;
        if router_result.is_err() {
            error!("router was closed before we could gracefully shutdown all sharders.");
        }
        self.router_handle.await?
    }

    async fn inner_log(&mut self, cmd: ShardCommand) -> anyhow::Result<()> {
        self.router_sender
            .send(cmd)
            .await
            .map_err(|_e| anyhow::anyhow!("failed to route"))
    }

    pub async fn log_account_update(&mut self, update: AccountUpdate) -> anyhow::Result<()> {
        let cmd = ShardCommand::InsertAccountUpdate(update);
        self.inner_log(cmd).await
    }

    pub async fn log_transaction(&mut self, tx: Transaction) -> anyhow::Result<()> {
        let cmd = ShardCommand::InsertTransaction(tx);
        self.inner_log(cmd).await
    }
}
