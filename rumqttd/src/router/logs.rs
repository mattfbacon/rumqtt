use super::{Ack, Connection};
use slab::Slab;
use tracing::trace;

use crate::protocol::{
    matches, ConnAck, Packet, PingResp, PubAck, PubComp, PubRec, PubRel, Publish, SubAck, UnsubAck,
};
use crate::router::{DataRequest, FilterIdx, SubscriptionMeter, Waiters};
use crate::{ConnectionId, Cursor, Filter, Offset, RouterConfig, Topic};

use crate::segments::{CommitLog, Position};
use crate::Storage;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;

/// Stores 'device' data and 'actions' data in native commitlog
/// organized by subscription filter. Device data is replicated
/// while actions data is not
pub struct DataLog {
    pub config: RouterConfig,
    /// Native commitlog data organized by subscription. Contains
    /// device data and actions data logs.
    ///
    /// Device data is replicated while actions data is not.
    /// Also has waiters used to wake connections/replicator tracker
    /// which are caught up with all the data on 'Filter' and waiting
    /// for new data
    pub native: Slab<Data<Publish>>,
    /// Map of subscription filter name to filter index
    filter_indexes: HashMap<Filter, FilterIdx>,
    retained_publishes: HashMap<Topic, Publish>,
    /// List of filters associated with a topic
    publish_filters: HashMap<Topic, Vec<FilterIdx>>,
    pub filter_read_markers: HashMap<FilterIdx, ReadMarker>,
    pub filter_write_markers: HashMap<FilterIdx, HashSet<ConnectionId>>,
}

#[derive(Default)]
pub struct ReadMarker {
    subscriber_markers: HashMap<ConnectionId, Offset>,
    slowest_marker: Option<Offset>,
}

impl ReadMarker {
    // Return true if slowest_marker moved ahead
    pub fn update_subscriber_marker(
        &mut self,
        subscriber_id: ConnectionId,
        marker: Offset,
    ) -> bool {
        // after this operations the slowest_marker >= pre operations slowest_marker
        *self.subscriber_markers.entry(subscriber_id).or_default() = marker;
        self.compute_slowest_marker()
    }

    // Return true if slowest_marker moved ahead
    fn compute_slowest_marker(&mut self) -> bool {
        let prev_slowest_marker = self.slowest_marker;
        self.slowest_marker = self.subscriber_markers.values().min().copied();

        self.slowest_marker > prev_slowest_marker
    }

    pub fn get_slowest_marker(&self) -> Option<Offset> {
        self.slowest_marker
    }
}

impl DataLog {
    pub fn new(config: RouterConfig) -> io::Result<DataLog> {
        let mut native = Slab::new();
        let mut filter_indexes = HashMap::new();
        let retained_publishes = HashMap::new();
        let publish_filters = HashMap::new();
        let filter_read_markers = HashMap::new();
        let filter_write_markers = HashMap::new();

        if let Some(warmup_filters) = config.initialized_filters.clone() {
            for filter in warmup_filters {
                let data = Data::new(&filter, config.max_segment_size, config.max_segment_count);

                // Add commitlog to datalog and add datalog index to filter to
                // datalog index map
                let idx = native.insert(data);
                filter_indexes.insert(filter, idx);
            }
        }

        Ok(DataLog {
            config,
            native,
            publish_filters,
            filter_indexes,
            retained_publishes,
            filter_read_markers,
            filter_write_markers,
        })
    }

    pub fn meter(&self, filter: &str) -> Option<SubscriptionMeter> {
        self.native
            .get(*self.filter_indexes.get(filter)?)
            .map(|data| data.meter.clone())
    }

    pub fn waiters(&self, filter: &Filter) -> Option<&Waiters<DataRequest>> {
        self.native
            .get(*self.filter_indexes.get(filter)?)
            .map(|data| &data.waiters)
    }

    pub fn remove_waiters_for_id(
        &mut self,
        id: ConnectionId,
        filter: &Filter,
    ) -> Option<DataRequest> {
        let data = self
            .native
            .get_mut(*self.filter_indexes.get(filter)?)
            .unwrap();
        let waiters = data.waiters.get_mut();

        waiters
            .iter()
            .position(|&(conn_id, _)| conn_id == id)
            .and_then(|index| {
                waiters
                    .swap_remove_back(index)
                    .map(|(_, data_req)| data_req)
            })
    }

    // TODO: Currently returning a Option<Vec> instead of Option<&Vec> due to Rust borrow checker
    // limitation
    pub fn matches(&mut self, topic: &str) -> Option<Vec<usize>> {
        match &self.publish_filters.get(topic) {
            Some(v) => Some(v.to_vec()),
            None => {
                let v: Vec<usize> = self
                    .filter_indexes
                    .iter()
                    .filter(|(filter, _)| matches(topic, filter))
                    .map(|(_, filter_idx)| *filter_idx)
                    .collect();

                if !v.is_empty() {
                    self.publish_filters.insert(topic.to_owned(), v.clone());
                }

                Some(v)
            }
        }
    }

    pub fn next_native_offset(&mut self, filter: &str) -> (FilterIdx, Offset) {
        let publish_filters = &mut self.publish_filters;
        let filter_indexes = &mut self.filter_indexes;

        let (filter_idx, data) = match filter_indexes.get(filter) {
            Some(idx) => (*idx, self.native.get(*idx).unwrap()),
            None => {
                let data = Data::new(
                    filter,
                    self.config.max_segment_size,
                    self.config.max_segment_count,
                );

                // Add commitlog to datalog and add datalog index to filter to
                // datalog index map
                let idx = self.native.insert(data);
                self.filter_indexes.insert(filter.to_owned(), idx);

                // Match new filter to existing topics and add to publish_filters if it matches
                for (topic, filters) in publish_filters.iter_mut() {
                    if matches(topic, filter) {
                        filters.push(idx);
                    }
                }

                (idx, self.native.get(idx).unwrap())
            }
        };

        (filter_idx, data.log.next_offset())
    }

    pub fn native_readv(
        &self,
        filter_idx: FilterIdx,
        offset: Offset,
        len: u64,
    ) -> io::Result<(Position, Vec<Publish>)> {
        // unwrap to get index of `self.native` is fine here, because when a new subscribe packet
        // arrives in `Router::handle_device_payload`, it first calls the function
        // `next_native_offset` which creates a new commitlog if one doesn't exist. So any new
        // reads will definitely happen on a valid filter.
        let data = self.native.get(filter_idx).unwrap();
        let mut o = Vec::new();
        // TODO: `readv` is infallible but its current return type does not
        // reflect that. Consequently, this method is also infallible.
        // Encoding this information is important so that calling function
        // has more information on how this method behaves.
        let next = data.log.readv(offset, len, &mut o)?;
        Ok((next, o))
    }

    pub fn shadow(&mut self, filter: &str) -> Option<Publish> {
        let data = self.native.get_mut(*self.filter_indexes.get(filter)?)?;
        data.log.last()
    }

    /// This method is called when the subscriber has caught up with the commit log. In which case,
    /// instead of actively checking for commits in each `Router::run_inner` iteration, we instead
    /// wait and only try reading again when new messages have been added to the commit log. This
    /// methods converts a `DataRequest` (which actively reads the commit log in `Router::consume`)
    /// to a `Waiter` (which only reads when notified).
    pub fn park(&mut self, id: ConnectionId, request: DataRequest) {
        // calling unwrap on index here is fine, because only place this function is called is in
        // `Router::consume` method, when the status after reading from commit log of the same
        // filter as `request` is "done", that is, the subscriber has caught up. In other words,
        // there has been atleast 1 call to `native_readv` for the same filter, which means if
        // `native_readv` hasn't paniced, so this won't panic either.
        let data = self.native.get_mut(request.filter_idx).unwrap();
        data.waiters.register(id, request);
    }

    /// Cleanup a connection from all the waiters
    pub fn clean(&mut self, id: ConnectionId) -> Vec<DataRequest> {
        let mut inflight = Vec::new();
        for (_, data) in self.native.iter_mut() {
            inflight.append(&mut data.waiters.remove(id));
        }

        inflight
    }

    pub fn insert_to_retained_publishes(&mut self, publish: Publish, topic: Topic) {
        self.retained_publishes.insert(topic, publish);
    }

    pub fn remove_from_retained_publishes(&mut self, topic: Topic) {
        self.retained_publishes.remove(&topic);
    }

    pub fn handle_retained_messages(
        &mut self,
        filter: &str,
        notifications: &mut VecDeque<(ConnectionId, DataRequest)>,
    ) {
        trace!(info = "retain-msg", filter = &filter);

        let idx = self.filter_indexes.get(filter).unwrap();

        let datalog = self.native.get_mut(*idx).unwrap();

        for (topic, publish) in self.retained_publishes.iter_mut() {
            if matches(topic, filter) {
                datalog.append(publish.clone(), notifications);
            }
        }
    }

    /// Make a note of subscribers reading from current topic
    pub fn register_subscriber(
        &mut self,
        filter_id: usize,
        start_cursor: Offset,
        subscriber_id: ConnectionId,
    ) {
        // reset the read marker of the filter to the new
        // let read_marker = ReadMarker {
        //     start_pos: start_cursor,
        //     curr_pos: start_cursor,
        // };

        let marker = self.filter_read_markers.entry(filter_id).or_default();
        marker.update_subscriber_marker(subscriber_id, start_cursor);
    }
}

pub struct Data<T> {
    filter: Filter,
    log: CommitLog<T>,
    waiters: Waiters<DataRequest>,
    meter: SubscriptionMeter,
}

impl<T> Data<T>
where
    T: Storage + Clone,
{
    fn new(filter: &str, max_segment_size: usize, max_mem_segments: usize) -> Data<T> {
        let log = CommitLog::new(max_segment_size, max_mem_segments).unwrap();

        let waiters = Waiters::with_capacity(10);
        let metrics = SubscriptionMeter::default();
        Data {
            filter: filter.to_owned(),
            log,
            waiters,
            meter: metrics,
        }
    }

    /// Writes to all the filters that are mapped to this publish topic
    /// and wakes up consumers that are matching this topic (if they exist)
    pub fn append(
        &mut self,
        item: T,
        notifications: &mut VecDeque<(ConnectionId, DataRequest)>,
    ) -> (Offset, &Filter) {
        let size = item.size();
        let offset = self.log.append(item);
        if let Some(mut parked) = self.waiters.take() {
            notifications.append(&mut parked);
        }

        self.meter.count += 1;
        self.meter.append_offset = offset;
        self.meter.total_size += size;
        self.meter.head_and_tail_id = self.log.head_and_tail();

        (offset, &self.filter)
    }
}

/// Acks log for a subscription
#[derive(Debug)]
pub struct AckLog {
    // Committed acks per connection. First pkid, last pkid, data
    committed: VecDeque<Ack>,
    // Recorded qos 2 publishes
    recorded: VecDeque<Publish>,
    deferred_acks: VecDeque<DeferredAck>,
}

/// Offset map, for topic T:
///
/// - For N publishes on T with M matching filters, we store the offsets
/// in a map. The i, j value of map gives the offset of i-th publish on
/// j-th filter's append log.
/// - We also store markers per filter. For j-th filter, the marker tells
/// what was the last persisted offset.
/// - Based on the markers of all filters in an offset map we calculate a
/// threshold. Threshold points to the latest publish packet that has been
/// persisted by all the subscribers.
///
/// From the datalog we receive information that F_i has been updated.
/// On update of marker of F_i, we recompute threshold for all offset
/// maps that have F_i in it and release the pubacks based new threshold.  
///
/// Example:
/// For publish on topic a/b/c we have have the following offset map.
/// Topic a/b/c maps to the filters a/b/c, a/+/c, and a/#. We have 4
/// publishes on 3 filters and marker (denoted by ←) corresponding to
/// every filter. Puback for P_0 is already released.
///
///              filters ➡️
///    publishes
///        ↓  
///            |  F_0     F_1     F_2
///            |  a/b/c   a/+/c   a/#
///      ------------------------------             
///   [x] P_0  |  0 ←     3       1          ⬅️ threshold
///       P_1  |  1       5 ←     10 ←         
///       P_2  |  2       7       20
///       P_3  |  3       10      22
///
///
/// Lets say markers were updated as following:
///
///             old     new
///      F_0    0       3
///      F_1    5       8
///      F_2    11      21
///
/// We recompute the threshold and release pubacks for P_1 and P_2:
///
///              filters ➡️
///    publishes
///        ↓  
///            |  F_0     F_1     F_2
///            |  a/b/c   a/+/c   a/#
///      ------------------------------             
///   [x] P_0  |  0       3       1          ⬅️ old threshold
///   [x] P_1  |  1       5       10
///   [x] P_2  |  2       7 ←     20         ⬅️ new threshold
///       P_3  |  3 ←     10      22 ←
///   
#[derive(Debug)]
struct DeferredAck {
    puback: VecDeque<PubAck>,
    // store of offsets of publishes on filters
    // VecDeque<Offset> is increasing in nature
    filter_publish_markers: HashMap<FilterIdx, VecDeque<Offset>>,
    // store of offsets till which all subscribers have persisted data
    filter_thresholds: HashMap<FilterIdx, Offset>,
}

impl AckLog {
    /// New log
    pub fn new() -> AckLog {
        AckLog {
            committed: VecDeque::with_capacity(100),
            recorded: VecDeque::with_capacity(100),
            deferred_acks: VecDeque::with_capacity(100),
        }
    }

    pub fn connack(&mut self, id: ConnectionId, ack: ConnAck) {
        let ack = Ack::ConnAck(id, ack);
        self.committed.push_back(ack);
    }

    pub fn suback(&mut self, ack: SubAck) {
        let ack = Ack::SubAck(ack);
        self.committed.push_back(ack);
    }

    fn puback(&mut self, ack: PubAck) {
        let ack = Ack::PubAck(ack);
        self.committed.push_back(ack);
    }

    pub fn pubrec(&mut self, publish: Publish, ack: PubRec) {
        let ack = Ack::PubRec(ack);
        self.recorded.push_back(publish);
        self.committed.push_back(ack);
    }

    pub fn pubrel(&mut self, ack: PubRel) {
        let ack = Ack::PubRel(ack);
        self.committed.push_back(ack);
    }

    pub fn pubcomp(&mut self, ack: PubComp) -> Option<Publish> {
        let ack = Ack::PubComp(ack);
        self.committed.push_back(ack);
        self.recorded.pop_front()
    }

    pub fn pingresp(&mut self, ack: PingResp) {
        let ack = Ack::PingResp(ack);
        self.committed.push_back(ack);
    }

    pub fn unsuback(&mut self, ack: UnsubAck) {
        let ack = Ack::UnsubAck(ack);
        self.committed.push_back(ack);
    }

    pub fn readv(&mut self) -> &mut VecDeque<Ack> {
        &mut self.committed
    }

    pub fn insert_pending_acks(&mut self, puback: PubAck, offset_map: HashMap<usize, Offset>) {
        // do something
    }
}

#[cfg(test)]
mod test {
    use super::DataLog;
    use crate::RouterConfig;

    #[test]
    fn publish_filters_updating_correctly_on_new_topic_subscription() {
        let config = RouterConfig {
            instant_ack: true,
            max_segment_size: 1024,
            max_connections: 10,
            max_segment_count: 10,
            max_read_len: 1024,
            initialized_filters: None,
        };
        let mut data = DataLog::new(config).unwrap();
        data.next_native_offset("topic/a");
        data.matches("topic/a");

        data.next_native_offset("topic/+");

        assert_eq!(data.publish_filters.get("topic/a").unwrap().len(), 2);
    }

    #[test]
    fn publish_filters_updating_correctly_on_new_publish() {
        let config = RouterConfig {
            instant_ack: true,
            max_segment_size: 1024,
            max_connections: 10,
            max_segment_count: 10,
            max_read_len: 1024,
            initialized_filters: None,
        };
        let mut data = DataLog::new(config).unwrap();
        data.next_native_offset("+/+");

        data.matches("topic/a");

        assert_eq!(data.publish_filters.get("topic/a").unwrap().len(), 1);
    }

    //     #[test]
    //     fn appends_are_written_to_correct_commitlog() {
    //         pretty_env_logger::init();
    //         let config = RouterConfig {
    //             instant_ack: true,
    //             max_segment_size: 1024,
    //             max_connections: 10,
    //             max_mem_segments: 10,
    //             max_disk_segments: 0,
    //             max_read_len: 1024,
    //             log_dir: None,
    //             dynamic_log: true,
    //         };

    //         let mut data = DataLog::new(config).unwrap();
    //         data.next_native_offset("/devices/2321/actions");
    //         for i in 0..2 {
    //             let publish = Publish::new("/devices/2321/events/imu/jsonarray", QoS::AtLeastOnce, vec![1, 2, 3]);
    //             let v = data.native_append(publish);
    //             dbg!(v);
    //         }

    //         for i in 0..2 {
    //             let publish = Publish::new("/devices/2321/actions", QoS::AtLeastOnce, vec![1, 2, 3]);
    //             let v = data.native_append(publish);
    //             dbg!(v);
    //         }
    //     }
}
