
//TODO use faster HashMap, HashSet
use std::{self, iter, mem};
use std::collections::VecDeque;
use std::collections::hash_map;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::mpsc;
use std::u32;

use mio;

use packets::*;
use async::store::AsyncStoreClient;
use self::FromStore::*;
use self::FromClient::*;

use hash::HashMap;

pub mod log_handle;

#[cfg(test)]
mod tests;

const MAX_PREFETCH: u32 = 8;

type ChainEntry = Rc<Vec<u8>>;

pub struct ThreadLog {
    to_store: mio::channel::Sender<Vec<u8>>, //TODO send WriteState or other enum?
    from_outside: mpsc::Receiver<Message>, //TODO should this be per-chain?
    blockers: HashMap<OrderIndex, Vec<ChainEntry>>,
    blocked_multiappends: HashMap<Uuid, MultiSearchState>,
    per_chains: HashMap<order, PerChain>,
    //TODO replace with queue from deque to allow multiple consumers
    ready_reads: mpsc::Sender<Vec<u8>>,
    //TODO blocked_chains: BitSet ?
    //TODO how to multiplex writers finished_writes: Vec<mpsc::Sender<()>>,
    finished_writes: mpsc::Sender<(Uuid, Vec<OrderIndex>)>,
    //FIXME is currently unused
    to_return: VecDeque<Vec<u8>>,
    //TODO
    no_longer_blocked: Vec<OrderIndex>,
    cache: BufferCache,
    chains_currently_being_read: IsRead,
    num_snapshots: usize,
}

//TODO we could add messages from the client on read, and keep a counter of messages sent
//     this would allow us to ensure that every client gets an end-of-data message, as long
//     ad there're no concurrent snapshots...
struct PerChain {
    //TODO repr?
    //blocking: HashMap<entry, OrderIndex>,
    //read: VecDeque<ChainEntry>,
    //searching_for_multi_appends: HashMap<Uuid, OrderIndex>,
    //found_sentinels: HashSet<Uuid>,
    chain: order,
    last_snapshot: entry,
    last_read_sent_to_server: entry,
    outstanding_reads: u32, //TODO what size should this be
    //TODO is this necessary first_buffered: entry,
    last_returned_to_client: entry,
    blocked_on_new_snapshot: Option<Vec<u8>>,
    //TODO this is where is might be nice to have a more structured id format
    found_but_unused_multiappends: HashMap<Uuid, entry>,
    is_being_read: Option<ReadState>,
    is_interesting: bool,
}

struct ReadState {
    outstanding_snapshots: u32,
    num_multiappends_searching_for: usize,
    _being_read: IsRead,
}

impl ReadState {
    fn new(being_read: &IsRead) -> Self {
        ReadState {
            outstanding_snapshots: 0,
            num_multiappends_searching_for: 0,
            _being_read: being_read.clone(),
        }
    }
}

type IsRead = Rc<ReadHandle>;

#[derive(Debug)]
struct ReadHandle;

struct MultiSearchState {
    val: Vec<u8>,
    pieces_remaining: usize,
}

pub enum Message {
    FromStore(FromStore),
    FromClient(FromClient),
}

//TODO hide in struct
pub enum FromStore {
    WriteComplete(Uuid, Vec<OrderIndex>), //TODO
    ReadComplete(OrderIndex, Vec<u8>),
}

pub enum FromClient {
    //TODO
    SnapshotAndPrefetch(order),
    PerformAppend(Vec<u8>),
    ReturnBuffer(Vec<u8>),
    Shutdown,
}

enum MultiSearch {
    Finished(Vec<u8>),
    InProgress,
    EarlySentinel,
    BeyondHorizon(Vec<u8>),
    Repeat,
    //MultiSearch::FirstPart(),
}

//TODO no-alloc
struct BufferCache {
    vec_cache: VecDeque<Vec<u8>>,
    //     rc_cache: VecDeque<Rc<Vec<u8>>>,
    //     alloced: usize,
    //     avg_alloced: usize,
}

impl ThreadLog {

    //TODO
    pub fn new<I>(to_store: mio::channel::Sender<Vec<u8>>,
        from_outside: mpsc::Receiver<Message>,
        ready_reads: mpsc::Sender<Vec<u8>>,
        finished_writes: mpsc::Sender<(Uuid, Vec<OrderIndex>)>,
        interesting_chains: I)
    -> Self
    where I: IntoIterator<Item=order>{
        ThreadLog {
            to_store: to_store,
            from_outside: from_outside,
            blockers: Default::default(),
            blocked_multiappends: Default::default(),
            ready_reads: ready_reads,
            finished_writes: finished_writes,
            per_chains: interesting_chains.into_iter().map(|c| (c, PerChain::interesting(c))).collect(),
            to_return: Default::default(),
            no_longer_blocked: Default::default(),
            cache: BufferCache::new(),
            chains_currently_being_read: Rc::new(ReadHandle),
            num_snapshots: 0,
        }
    }

    pub fn run(mut self) {
        loop {
            let msg = self.from_outside.recv().expect("outside is gone");
            if !self.handle_message(msg) { return }
        }
    }

    fn handle_message(&mut self, msg: Message) -> bool {
        match msg {
            Message::FromClient(msg) => self.handle_from_client(msg),
            Message::FromStore(msg) => self.handle_from_store(msg),
        }
    }

    fn handle_from_client(&mut self, msg: FromClient) -> bool {
        match msg {
            SnapshotAndPrefetch(chain) => {
                trace!("FUZZY snapshot");
                self.num_snapshots = self.num_snapshots.saturating_add(1);
                //FIXME
                if chain != 0.into() {
                    self.fetch_snapshot(chain);
                    self.prefetch(chain);
                }
                else {
                    let chains: Vec<_> = self.per_chains.iter()
                        .filter(|pc| pc.1.is_interesting)
                        .map(|pc| pc.0.clone()).collect();
                    for chain in chains {
                        self.fetch_snapshot(chain);
                        self.prefetch(chain);
                    }
                }
                true
            }
            PerformAppend(msg) => {
                {
                    let layout = bytes_as_entry(&msg).kind.layout();
                    assert!(layout == EntryLayout::Data || layout == EntryLayout::Multiput);
                }
                self.to_store.send(msg).expect("store hung up");
                true
            }
            ReturnBuffer(buffer) => {
                self.cache.cache_buffer(buffer);
                true
            }
            Shutdown => {
                //TODO send shutdown
                false
            }
        }
    }

    fn handle_from_store(&mut self, msg: FromStore) -> bool {
        match msg {
            WriteComplete(id, locs) =>
                self.finished_writes.send((id, locs)).expect("client is gone"),
            ReadComplete(loc, msg) => self.handle_completed_read(loc, msg),
        }
        true
    }

    fn fetch_snapshot(&mut self, chain: order) {
        //XXX outstanding_snapshots is incremented in prefetch
        let packet = self.make_read_packet(chain, u32::MAX.into());
        self.to_store.send(packet).expect("store hung up")
    }

    fn prefetch(&mut self, chain: order) {
        //TODO allow new chains?
        //TODO how much to fetch
        let num_to_fetch = {
            let pc = &mut self.per_chains.get_mut(&chain).expect("boring server read");
            pc.increment_outstanding_snapshots(&self.chains_currently_being_read);
            let num_to_fetch = pc.num_to_fetch();
            let num_to_fetch = std::cmp::max(num_to_fetch, MAX_PREFETCH);
            let currently_buffering = pc.currently_buffering();
            //FIXME use outstanding reads
            if currently_buffering < num_to_fetch { num_to_fetch - currently_buffering }
            else { 0 }
        };
        for _ in 0..num_to_fetch {
            self.fetch_next(chain)
        }
    }

    fn handle_completed_read(&mut self, read_loc: OrderIndex, msg: Vec<u8>) {
        //TODO right now this assumes order...
        let kind = bytes_as_entry(&msg).kind;
        trace!("FUZZY handle read @ {:?}", read_loc);

        match kind.layout() {
            EntryLayout::Read => {
                trace!("FUZZY read has no data");
                debug_assert!(!kind.contains(EntryKind::ReadSuccess));
                debug_assert!(bytes_as_entry(&msg).locs()[0] == read_loc);
                if read_loc.1 < u32::MAX.into() {
                    trace!("FUZZY overread at {:?}", read_loc);
                    //TODO would be nice to handle ooo reads better...
                    //     we can probably do it by checking (chain, read_loc - 1)
                    //     to see if the read we're about to attempt is there, but
                    //     it might be better to switch to a buffer per-chain model
                    self.per_chains.get_mut(&read_loc.0).map(|s| {
                        s.overread_at(read_loc.1);
                        s.outstanding_reads -= 1;
                    });
                }
                else {
                    let unblocked = self.per_chains.get_mut(&read_loc.0).and_then(|s| {
                        let e = bytes_as_entry(&msg);
                        assert_eq!(e.locs()[0].1, u32::MAX.into());
                        debug_assert!(!e.kind.contains(EntryKind::ReadSuccess));
                        let new_horizon = e.dependencies()[0].1;
                        trace!("FUZZY try update horizon to {:?}", (read_loc.0, new_horizon));
                        s.decrement_outstanding_snapshots();
                        s.update_horizon(new_horizon)
                    });
                    if let Some(val) = unblocked {
                        let locs = self.return_entry(val);
                        if let Some(locs) = locs { self.stop_blocking_on(locs) }
                    }
                }
            }
            EntryLayout::Data => {
                trace!("FUZZY read is single");
                debug_assert!(kind.contains(EntryKind::ReadSuccess));
                //assert!(read_loc.1 >= pc.first_buffered);
                //TODO check that read is needed?
                //TODO no-alloc?
                self.per_chains.get_mut(&read_loc.0).map(|s| s.outstanding_reads -= 1);
                let packet = Rc::new(msg);
                self.add_blockers(&packet);
                self.try_returning_at(read_loc, packet);
            }
            layout @ EntryLayout::Multiput | layout @ EntryLayout::Sentinel => {
                trace!("FUZZY read is multi");
                debug_assert!(kind.contains(EntryKind::ReadSuccess));
                self.per_chains.get_mut(&read_loc.0).map(|s| s.outstanding_reads -= 1);
                let is_sentinel = layout == EntryLayout::Sentinel;
                let search_status = self.update_multi_part_read(read_loc, msg, is_sentinel);
                match search_status {
                    MultiSearch::InProgress | MultiSearch::EarlySentinel => {}
                    MultiSearch::BeyondHorizon(..) => {
                        //TODO better ooo reads
                        self.per_chains.entry(read_loc.0)
                            .or_insert_with(|| PerChain::new(read_loc.0))
                            .overread_at(read_loc.1);
                    }
                    MultiSearch::Finished(msg) => {
                        //TODO no-alloc?
                        let packet = Rc::new(msg);
                        //TODO it would be nice to fetch the blockers in parallel...
                        //     we can add a fetch blockers call in update_multi_part_read
                        //     which updates the horizon but doesn't actually add the block
                        self.add_blockers(&packet);
                        self.try_returning(packet);
                    }
                    MultiSearch::Repeat => {}
                }
            }

            EntryLayout::Lock => unreachable!(),
        }

        let finished_server = self.continue_fetch_if_needed(read_loc.0);
        if finished_server {
            trace!("FUZZY finished reading {:?}", read_loc.0);

            self.per_chains.get_mut(&read_loc.0).map(|pc| {
                debug_assert!(pc.is_finished());
                trace!("FUZZY chain {:?} is finished", pc.chain);
                pc.is_being_read = None
            });
            if self.finshed_reading() {
                trace!("FUZZY finished reading all chains");
                //FIXME add is_snapshoting to PerChain so this doesn't race?
                trace!("FUZZY finished reading");
                //TODO do we need a better system?
                let num_completeds = mem::replace(&mut self.num_snapshots, 0);
                //assert!(num_completeds > 0);
                for _ in 0..num_completeds {
                    let _ = self.ready_reads.send(vec![]);
                }
            }
        }
        else {
            //#[cfg(debug_assertions)]
            //self.per_chains.get(&read_loc.0).map(|pc| {
            //    trace!("chain {:?} not finished, " pc.outstanding_reads, pc.last_returned)
            //});
        }
    }

    /// Blocks a packet on entries a it depends on. Will increment the refcount for each
    /// blockage.
    fn add_blockers(&mut self, packet: &ChainEntry) {
        //FIXME dependencies currently assumes you gave it the correct type
        //      this is unnecessary and should be changed
        let entr = bytes_as_entry(packet);
        let deps = entr.dependencies();
        let locs = entr.locs();
        trace!("FUZZY checking {:?} for blockers in {:?}", locs, deps);
        for &OrderIndex(chain, index) in deps {
            let blocker_already_returned = self.per_chains.get_mut(&chain)
                .expect("read uninteresting chain")
                .has_returned(index);
            if !blocker_already_returned {
                trace!("FUZZY read @ {:?} blocked on {:?}", locs, (chain, index));
                //TODO no-alloc?
                let blocked = self.blockers.entry(OrderIndex(chain, index))
                    .or_insert_with(Vec::new);
                blocked.push(packet.clone());
            } else {
                trace!("FUZZY read @ {:?} need not wait for {:?}", locs, (chain, index));
            }
        }
        for &loc in locs {
            if loc.0 == order::from(0) { continue }
            let (is_next_in_chain, needs_to_be_returned) = {
                let pc = self.per_chains.get(&loc.0).expect("fetching uninteresting chain");
                (pc.next_return_is(loc.1), !pc.has_returned(loc.1))
            };
            if !is_next_in_chain && needs_to_be_returned {
                self.enqueue_packet(loc, packet.clone());
            }
        }
    }

    fn fetch_blockers_if_needed(&mut self, packet: &ChainEntry) {
        //TODO num_to_fetch
        //FIXME only do if below last_snapshot?
        let deps = bytes_as_entry(packet).dependencies();
        for &OrderIndex(chain, index) in deps {
            let unblocked;
            let num_to_fetch: u32 = {
                let pc = self.per_chains.get_mut(&chain)
                    .expect("tried reading uninteresting chain");
                unblocked = pc.update_horizon(index);
                pc.num_to_fetch()
            };
            trace!("FUZZY blocker {:?} needs {:?} additional reads", chain, num_to_fetch);
            for _ in 0..num_to_fetch {
                self.fetch_next(chain)
            }
            if let Some(val) = unblocked {
                let locs = self.return_entry(val);
                if let Some(locs) = locs { self.stop_blocking_on(locs) }
            }
        }
    }

    fn try_returning_at(&mut self, loc: OrderIndex, packet: ChainEntry) {
        match Rc::try_unwrap(packet) {
            Ok(e) => {
                trace!("FUZZY read {:?} is next", loc);
                if self.return_entry_at(loc, e) {
                    self.stop_blocking_on(iter::once(loc));
                }
            }
            //TODO should this be in add_blockers?
            Err(e) => self.fetch_blockers_if_needed(&e),
        }
    }

    fn try_returning(&mut self, packet: ChainEntry) {
        match Rc::try_unwrap(packet) {
            Ok(e) => {
                trace!("FUZZY returning next read?");
                if let Some(locs) = self.return_entry(e) {
                    trace!("FUZZY {:?} unblocked", locs);
                    self.stop_blocking_on(locs);
                }
            }
            //TODO should this be in add_blockers?
            Err(e) => self.fetch_blockers_if_needed(&e),
        }
    }

    fn stop_blocking_on<I>(&mut self, locs: I)
    where I: IntoIterator<Item=OrderIndex> {
        for loc in locs {
            if loc.0 == order::from(0) { continue }
            trace!("FUZZY unblocking reads after {:?}", loc);
            self.try_return_blocked_by(loc);
        }
        while let Some(loc) = self.no_longer_blocked.pop() {
            trace!("FUZZY continue unblocking reads after {:?}", loc);
            self.try_return_blocked_by(loc);
        }
    }

    fn try_return_blocked_by(&mut self, loc: OrderIndex) {
        //FIXME switch to using try_returning so needed fetches are done
        //      move up the stop_block loop into try_returning?
        let blocked = self.blockers.remove(&loc);
        if let Some(blocked) = blocked {
            for blocked in blocked.into_iter() {
                match Rc::try_unwrap(blocked) {
                    Ok(val) => {
                        {
                            let locs = bytes_as_entry(&val).locs();
                            trace!("FUZZY {:?} unblocked by {:?}", locs, loc);
                            self.no_longer_blocked.extend_from_slice(locs);
                        }
                        self.return_entry(val);
                    }
                    Err(still_blocked) =>
                        trace!("FUZZY {:?} no longer by {:?} but still blocked",
                            bytes_as_entry(&still_blocked).locs(), loc),
                }
            }
        }
    }

    fn update_multi_part_read(&mut self,
        read_loc: OrderIndex,
        mut msg: Vec<u8>,
        is_sentinel: bool)
    -> MultiSearch {
        let (id, num_pieces) = {
            let entr = bytes_as_entry(&msg);
            let id = entr.id;
            let locs = entr.locs();
            let num_pieces = locs.into_iter()
                .filter(|&&OrderIndex(o, i)| o != order::from(0))
                .count();
            trace!("FUZZY multi part read {:?} @ {:?}, {:?} pieces", id, locs, num_pieces);
            (id, num_pieces)
        };

        //TODO this should never really occur...
        if num_pieces == 1 {
            return MultiSearch::Finished(msg)
        }

        let is_later_piece = self.blocked_multiappends.contains_key(&id);
        let mut omsg = None;
        if !is_later_piece && !is_sentinel {
            {
                let pc = &self.per_chains[&read_loc.0];
                //FIXME I'm not sure if this is right
                if !pc.is_within_snapshot(read_loc.1) {
                    trace!("FUZZY read multi too early @ {:?}", read_loc);
                    return MultiSearch::BeyondHorizon(msg)
                }

                if pc.has_returned(read_loc.1) {
                    trace!("FUZZY duplicate multi @ {:?}", read_loc);
                    return MultiSearch::BeyondHorizon(msg)
                }
            }

            let mut pieces_remaining = num_pieces;
            trace!("FUZZY first part of multi part read");
            for &mut OrderIndex(o, ref mut i) in bytes_as_entry_mut(&mut msg).locs_mut() {
                if o != order::from(0) {
                    trace!("FUZZY fetching multi part @ {:?}?", (o, *i));
                    let early_sentinel = self.fetch_multi_parts(&id, o, *i);
                    if let Some(loc) = early_sentinel {
                        trace!("FUZZY no fetch @ {:?} sentinel already found", (o, *i));
                        assert!(loc != entry::from(0));
                        *i = loc;
                        pieces_remaining -= 1
                    } else if *i != entry::from(0) {
                        trace!("FUZZY multi shortcircuit @ {:?}", (o, *i));
                        pieces_remaining -= 1
                    }
                } else {
                    trace!("FUZZY no need to fetch multi part @ {:?}", (o, *i));
                }
            }

            if pieces_remaining == 0 {
                trace!("FUZZY all sentinels had already been found for {:?}", read_loc);
                return MultiSearch::Finished(msg)
            }

            trace!("FUZZY {:?} waiting for {:?} pieces", read_loc, pieces_remaining);
            self.blocked_multiappends.insert(id, MultiSearchState {
                val: msg,
                pieces_remaining: pieces_remaining
            });
        }
        else if !is_later_piece && is_sentinel {
            trace!("FUZZY early sentinel");
            self.per_chains.get_mut(&read_loc.0)
                .expect("boring chain")
                .add_early_sentinel(id, read_loc.1);
            return MultiSearch::EarlySentinel
        }
        else { omsg = Some(msg); trace!("FUZZY later part of multi part read"); }

        debug_assert!(self.per_chains[&read_loc.0].is_within_snapshot(read_loc.1));

        let was_blind_search;
        let finished = {
            if let hash_map::Entry::Occupied(mut found) = self.blocked_multiappends.entry(id) {
                let finished = {
                    let multi = found.get_mut();
                    //FIXME ensure this only happens if debug assertions
                    if let (Some(msg), false) = (omsg, is_sentinel) {
                        unsafe { debug_assert_eq!(data_bytes(&multi.val), data_bytes(&msg)) }
                    }
                    let loc_ptr = bytes_as_entry_mut(&mut multi.val)
                        .locs_mut().into_iter()
                        .find(|&&mut OrderIndex(o, _)| o == read_loc.0)
                        .unwrap();
                    //FIXME
                    was_blind_search = loc_ptr.1 == entry::from(0);
                    if !was_blind_search {
                        debug_assert_eq!(*loc_ptr, read_loc)
                    } else {
                        multi.pieces_remaining -= 1;
                        trace!("FUZZY multi pieces remaining {:?}", multi.pieces_remaining);
                        *loc_ptr = read_loc;
                    }

                    multi.pieces_remaining == 0
                };
                match finished {
                    true => Some(found.remove().val),
                    false => None,
                }
            }
            else { unreachable!() }
        };

        //self.found_multi_part(read_loc.0, read_loc.1, was_blind_search);
        if was_blind_search {
            trace!("FUZZY finished blind seach for {:?}", read_loc);
            let pc = self.per_chains.entry(read_loc.0)
                .or_insert_with(|| PerChain::new(read_loc.0));
            pc.decrement_multi_search();
        }

        match finished {
            Some(val) => {
                trace!("FUZZY finished multi part read");
                MultiSearch::Finished(val)
            }
            None => {
                trace!("FUZZY multi part read still waiting");
                MultiSearch::InProgress
            }
        }
    }

    fn fetch_multi_parts(&mut self, id: &Uuid, chain: order, index: entry) -> Option<entry> {
        //TODO argh, no-alloc
        let (unblocked, early_sentinel) = {
            let pc = self.per_chains.entry(chain)
                .or_insert_with(|| PerChain::new(chain));

            let early_sentinel = pc.take_early_sentinel(&id);
            let potential_new_horizon = match early_sentinel {
                Some(loc) => loc,
                None => index,
            };

            //perform a non blind search if possible
            //TODO less than ideal with new lock scheme
            //     lock index is always below color index, starting with a non-blind read
            //     based on the lock number should be balid, if a bit conservative
            //     this would require some way to fall back to a blind read,
            //     if the horizon was reached before the multi found
            if index != entry::from(0) /* && !pc.is_within_snapshot(index) */ {
                trace!("RRRRR non-blind search {:?} {:?}", chain, index);
                let unblocked = pc.update_horizon(potential_new_horizon);
                if pc.last_read_sent_to_server == index - 1 {
                    pc.last_read_sent_to_server = pc.last_read_sent_to_server + 1
                }
                (unblocked, early_sentinel)
            } else if early_sentinel.is_some() {
                trace!("RRRRR already found {:?} {:?}", chain, early_sentinel);
                //FIXME How does this interact with cached reads?
                (None, early_sentinel)
            } else {
                pc.increment_multi_search(&self.chains_currently_being_read);
                trace!("RRRRR blind search {:?}", chain);
                (None, None)
            }
        };
        self.continue_fetch_if_needed(chain);

        if let Some(unblocked) = unblocked {
            //TODO no-alloc
            let locs = self.return_entry(unblocked);
            if let Some(locs) = locs { self.stop_blocking_on(locs) }
        }
        early_sentinel
    }

    fn continue_fetch_if_needed(&mut self, chain: order) -> bool {
        //TODO num_to_fetch
        let (num_to_fetch, unblocked) = {
            let pc = self.per_chains.entry(chain).or_insert_with(|| PerChain::new(chain));
            let num_to_fetch = pc.num_to_fetch();
            //TODO should fetch == number of multis searching for
            if num_to_fetch == 0 && pc.is_searching_for_multi() && pc.outstanding_reads == 0 {
                trace!("FUZZY {:?} updating horizon due to multi search", chain);
                (1, pc.increment_horizon())
            }
            else {
                trace!("FUZZY {:?} needs {:?} additional reads", chain, num_to_fetch);
                (num_to_fetch, None)
            }
        };

        for _ in 0..num_to_fetch {
            //FIXME check if we have a cached version before issuing fetch
            //      laking this can cause unsound behzvior on multipart reads
            self.fetch_next(chain)
        }

        if let Some(unblocked) = unblocked {
            //TODO no-alloc
            let locs = self.return_entry(unblocked);
            if let Some(locs) = locs { self.stop_blocking_on(locs) }
        }

        self.server_is_finished(chain)
    }

    fn enqueue_packet(&mut self, loc: OrderIndex, packet: ChainEntry) {
        assert!(loc.1 > 1.into());
        debug_assert!(self.per_chains.get(&loc.0).unwrap().last_returned_to_client
            < loc.1 - 1,
            "tried to enqueue non enqueable entry {:?}; last returned {:?}",
            loc.1 - 1,
            self.per_chains.get(&loc.0).unwrap().last_returned_to_client,
        );
        let blocked_on = OrderIndex(loc.0, loc.1 - 1);
        trace!("FUZZY read @ {:?} blocked on prior {:?}", loc, blocked_on);
        //TODO no-alloc?
        let blocked = self.blockers.entry(blocked_on).or_insert_with(Vec::new);
        blocked.push(packet.clone());
    }

    fn return_entry_at(&mut self, loc: OrderIndex, val: Vec<u8>) -> bool {
        debug_assert!(bytes_as_entry(&val).locs()[0] == loc);
        debug_assert!(bytes_as_entry(&val).locs().len() == 1);
        trace!("FUZZY trying to return read @ {:?}", loc);
        let OrderIndex(o, i) = loc;

        let is_interesting = {
            let pc = self.per_chains.get_mut(&o).expect("fetching uninteresting chain");

            if pc.has_returned(i) {
                return false
            }

            if !pc.is_within_snapshot(i) {
                trace!("FUZZY blocking read @ {:?}, waiting for snapshot", loc);
                pc.block_on_snapshot(val);
                return false
            }

            trace!("QQQQQ setting returned {:?}", (o, i));
            pc.set_returned(i);
            pc.is_interesting
        };
        trace!("FUZZY returning read @ {:?}", loc);
        if is_interesting {
            //FIXME first_buffered?
            self.ready_reads.send(val).expect("client hung up");
        }
        true
    }

    ///returns None if return stalled Some(Locations which are now unblocked>) if return
    ///        succeeded
    //TODO it may make sense to change these funtions to add the returned messages to an
    //     internal ring which can be used to discover the unblocked entries before the
    //     messages are flushed to the client, as this would remove the intermidate allocation
    //     and it may be a bit nicer
    fn return_entry(&mut self, val: Vec<u8>) -> Option<Vec<OrderIndex>> {
        let (locs, is_interesting) = {
            let mut should_block_on = None;
            {
                let locs = bytes_as_entry(&val).locs();
                trace!("FUZZY trying to return read from {:?}", locs);
                for &OrderIndex(o, i) in locs.into_iter() {
                    if o == order::from(0) { continue }
                    let pc = self.per_chains.get_mut(&o).expect("fetching uninteresting chain");
                    if pc.has_returned(i) { return None }
                    if !pc.is_within_snapshot(i) {
                        trace!("FUZZY must block read @ {:?}, waiting for snapshot", (o, i));
                        should_block_on = Some(o);
                    }
                }
            }
            if let Some(o) = should_block_on {
                let pc = self.per_chains.get_mut(&o).expect("fetching uninteresting chain");
                pc.block_on_snapshot(val);
                return None
            }
            let mut is_interesting = false;
            let locs = bytes_as_entry(&val).locs();
            for &OrderIndex(o, i) in locs.into_iter() {
                if o == order::from(0) { continue }
                trace!("QQQQ setting returned {:?}", (o, i));
                let pc = self.per_chains.get_mut(&o).expect("fetching uninteresting chain");
                debug_assert!(pc.is_within_snapshot(i));
                pc.set_returned(i);
                is_interesting |= pc.is_interesting;
            }
            //TODO no-alloc
            //     a better solution might be to have this function push onto a temporary
            //     VecDeque who's head is used to unblock further entries, and is then sent
            //     to the client
            (locs.to_vec(), is_interesting)
        };
        trace!("FUZZY returning read @ {:?}", locs);
        if is_interesting {
            //FIXME first_buffered?
            self.ready_reads.send(val).expect("client hung up");
        }
        Some(locs)
    }

    fn fetch_next(&mut self, chain: order) {
        let next = {
            let per_chain = &mut self.per_chains.get_mut(&chain)
                .expect("fetching uninteresting chain");
            //assert!(per_chain.last_read_sent_to_server < per_chain.last_snapshot,
            //    "last_read_sent_to_server {:?} >= {:?} last_snapshot @ fetch_next",
            //    per_chain.last_read_sent_to_server, per_chain.last_snapshot,
            //);
            per_chain.last_read_sent_to_server = per_chain.last_read_sent_to_server + 1;
            per_chain.increment_outstanding_reads(&self.chains_currently_being_read);
            per_chain.last_read_sent_to_server
        };
        let packet = self.make_read_packet(chain, next);

        self.to_store.send(packet).expect("store hung up");
    }

    fn make_read_packet(&mut self, chain: order, index: entry) -> Vec<u8> {
        let mut buffer = self.cache.alloc();
        {
            let e = EntryContents::Data(&(), &[]).fill_vec(&mut buffer);
            e.kind = EntryKind::Read;
            e.locs_mut()[0] = OrderIndex(chain, index);
            debug_assert_eq!(e.data_bytes, 0);
            debug_assert_eq!(e.dependency_bytes, 0);
        }
        buffer
    }

    fn finshed_reading(&mut self) -> bool {
        let finished = Rc::get_mut(&mut self.chains_currently_being_read).is_some();
        //FIXME this is dumb, it might be better to have a counter of how many servers we are
        //      waiting for
        debug_assert_eq!({
            let mut currently_being_read = 0;
            for (_, pc) in self.per_chains.iter() {
                assert_eq!(pc.is_finished(), !pc.is_being_read.is_some());
                if !pc.is_finished() {
                    currently_being_read += 1
                }
                //still_reading |= pc.has_outstanding_reads()
            }
            // !still_reading == (self.servers_currently_being_read == 0)
            if finished != (currently_being_read == 0) {
                panic!("currently_being_read == {:?} @ finish {:?}",
                currently_being_read, finished);
            }
            currently_being_read == 0
        }, finished);

        finished
    }

    fn server_is_finished(&self, chain: order) -> bool {
        let pc = &self.per_chains[&chain];
        assert!(!(pc.outstanding_reads == 0
            && pc.last_read_sent_to_server < pc.last_snapshot));
        assert!(!(pc.is_searching_for_multi() && !pc.has_outstanding_reads()));
        pc.is_finished()
    }
}

impl PerChain {
    fn new(chain: order) -> Self {
        PerChain {
            chain: chain,
            last_snapshot: 0.into(),
            last_read_sent_to_server: 0.into(),
            outstanding_reads: 0,
            last_returned_to_client: 0.into(),
            blocked_on_new_snapshot: None,
            found_but_unused_multiappends: Default::default(),
            is_being_read: None,
            is_interesting: false,
        }
    }

    fn interesting(chain: order) -> Self {
        let mut s = Self::new(chain);
        s.is_interesting = true;
        s
    }

    #[inline(always)]
    fn set_returned(&mut self, index: entry) {
        assert!(self.next_return_is(index));
        assert!(index > self.last_returned_to_client);
        assert!(index <= self.last_snapshot);
        trace!("QQQQQ returning {:?}", (self.chain, index));
        self.last_returned_to_client = index;
        if self.is_finished() {
            trace!("QQQQQ {:?} is finished", self.chain);
            self.is_being_read = None
        }
    }

    fn overread_at(&mut self, index: entry) {
        // The conditional is needed because sends we sent before reseting
        // last_read_sent_to_server race future calls to this function
        if self.last_read_sent_to_server >= index
            && self.last_read_sent_to_server > self.last_returned_to_client {
            trace!("FUZZY resetting read loc for {:?} from {:?} to {:?}",
                self.chain, self.last_read_sent_to_server, index - 1);
            self.last_read_sent_to_server = index - 1;
        }
    }

    fn increment_outstanding_snapshots(&mut self, is_being_read: &IsRead) -> u32 {
        let out = match &mut self.is_being_read {
            &mut Some(ReadState {ref mut outstanding_snapshots, ..} ) => {
                //TODO saturating arith
                *outstanding_snapshots = *outstanding_snapshots + 1;
                *outstanding_snapshots
            }
            r @ &mut None => {
                let mut read_state = ReadState::new(is_being_read);
                read_state.outstanding_snapshots += 1;
                let outstanding_snapshots = read_state.outstanding_snapshots;
                *r = Some(read_state);
                outstanding_snapshots

            }
        };
        debug_assert!(self.is_being_read.is_some());
        out
    }

    fn decrement_outstanding_snapshots(&mut self) -> u32 {
        self.is_being_read.as_mut().map(|&mut ReadState {ref mut outstanding_snapshots, ..}|{
            //TODO saturating arith
            *outstanding_snapshots = *outstanding_snapshots - 1;
            *outstanding_snapshots
            //TODO should this set is_being_read to None when last_returned == last snap?
        }).expect("tried to decerement snapshots on a chain not being read")
    }

    fn increment_multi_search(&mut self, is_being_read: &IsRead) {
        let searching = match &mut self.is_being_read {
            &mut Some(ReadState {ref mut num_multiappends_searching_for, ..} ) => {
                //TODO saturating arith
                *num_multiappends_searching_for = *num_multiappends_searching_for + 1;
                *num_multiappends_searching_for
            }
            r @ &mut None => {
                let mut read_state = ReadState::new(is_being_read);
                read_state.num_multiappends_searching_for += 1;
                let num_multiappends_searching_for = read_state.num_multiappends_searching_for;
                *r = Some(read_state);
                num_multiappends_searching_for
            }
        };
        trace!("QQQQQ {:?} + now searching for {:?} multis", self.chain, searching);
    }

    fn decrement_multi_search(&mut self) {
        let num_search = self.is_being_read.as_mut().map(|&mut ReadState {ref mut num_multiappends_searching_for, ..}| {
            debug_assert!(*num_multiappends_searching_for > 0);
            //TODO saturating arith
            *num_multiappends_searching_for = *num_multiappends_searching_for - 1;
            *num_multiappends_searching_for
            //TODO should this set is_being_read to None when last_returned == last snap?
        }).expect("tried to decrement multi_search in a chain not being read");
        trace!("QQQQQ {:?} - now searching for {:?} multis",
            self.chain, num_search);
    }

    #[inline(always)]
    fn increment_outstanding_reads(&mut self, is_being_read: &IsRead) {
        self.outstanding_reads += 1;
        if self.is_being_read.is_none() {
            self.is_being_read = Some(ReadState::new(is_being_read));
        }
    }

    fn can_return(&self, index: entry) -> bool {
        self.next_return_is(index) && self.is_within_snapshot(index)
    }

    fn has_returned(&self, index: entry) -> bool {
        trace!{"QQQQQ last return for {:?}: {:?}", self.chain, self.last_returned_to_client};
        index <= self.last_returned_to_client
    }

    fn next_return_is(&self, index: entry) -> bool {
        trace!("QQQQQ check {:?} next return for {:?}: {:?}",
            index, self.chain, self.last_returned_to_client + 1);
        index == self.last_returned_to_client + 1
    }

    fn is_within_snapshot(&self, index: entry) -> bool {
        trace!("QQQQQ {:?}: {:?} <= {:?}", self.chain, index, self.last_snapshot);
        index <= self.last_snapshot
    }

    fn is_searching_for_multi(&self) -> bool {
        self.is_being_read.as_ref().map(|br|
            br.num_multiappends_searching_for > 0).unwrap_or(false)
    }

    fn increment_horizon(&mut self) -> Option<Vec<u8>> {
        let new_horizon = self.last_snapshot + 1;
        self.update_horizon(new_horizon)
    }

    fn update_horizon(&mut self, new_horizon: entry) -> Option<Vec<u8>> {
        if self.last_snapshot < new_horizon {
            trace!("FUZZY update horizon {:?}", (self.chain, new_horizon));
            self.last_snapshot = new_horizon;
            if self.last_read_sent_to_server > new_horizon {
                //see also fn overread_at
                self.last_read_sent_to_server = new_horizon;
            }
            if entry_is_unblocked(&self.blocked_on_new_snapshot, self.chain, new_horizon) {
                trace!("FUZZY unblocked entry");
                return mem::replace(&mut self.blocked_on_new_snapshot, None)
            }
        }
        else {
            trace!("FUZZY needless horizon update for {:?}: {:?} <= {:?}",
                self.chain, new_horizon, self.last_snapshot);
        }

        return None;

        fn entry_is_unblocked(val: &Option<Vec<u8>>, chain: order, new_horizon: entry) -> bool {
            val.as_ref().map_or(false, |v| {
                let locs = bytes_as_entry(v).locs();
                for &OrderIndex(o, i) in locs {
                    if o == chain && i <= new_horizon {
                        return true
                    }
                }
                false
            })
        }
    }

    fn block_on_snapshot(&mut self, val: Vec<u8>) {
        debug_assert!(bytes_as_entry(&val).locs().into_iter()
            .find(|&&OrderIndex(o, _)| o == self.chain).unwrap().1 == self.last_snapshot + 1);
        assert!(self.blocked_on_new_snapshot.is_none());
        self.blocked_on_new_snapshot = Some(val)
    }

    fn num_to_fetch(&self) -> u32 {
        use std::cmp::min;
        //TODO
        const MAX_PIPELINED: u32 = 1000;
        //TODO switch to saturating sub?
        assert!(self.last_returned_to_client <= self.last_snapshot,
            "FUZZY returned value early. {:?} should be less than {:?}",
            self.last_returned_to_client, self.last_snapshot);
        if self.last_read_sent_to_server < self.last_snapshot
            && self.outstanding_reads < MAX_PIPELINED {
            let needed_reads =
                (self.last_snapshot - self.last_read_sent_to_server.into()).into();
            let to_read = min(needed_reads, MAX_PIPELINED - self.outstanding_reads);
            to_read
            //std::cmp::min(
            //    (self.last_snapshot - self.last_read_sent_to_server.into()).into(),
            //    MAX_PIPELINED
            //)
        } else {
            0
        }
    }

    fn currently_buffering(&self) -> u32 {
        //TODO switch to saturating sub?
        let currently_buffering = self.last_read_sent_to_server
            - self.last_returned_to_client.into();
        let currently_buffering: u32 = currently_buffering.into();
        currently_buffering
    }

    fn add_early_sentinel(&mut self, id: Uuid, index: entry) {
        assert!(index != 0.into());
        let old = self.found_but_unused_multiappends.insert(id, index);
        //TODO I'm not sure this is correct with how we handle overreads
        //debug_assert!(old.is_none(),
        //    "double sentinel insert {:?}",
        //    (self.chain, index)
        //);
    }

    fn take_early_sentinel(&mut self, id: &Uuid) -> Option<entry> {
        self.found_but_unused_multiappends.remove(id)
    }

    fn has_outstanding_reads(&self) -> bool {
        self.outstanding_reads > 0
    }

    fn has_outstanding_snapshots(&self) -> bool {
        self.is_being_read.as_ref().map(|&ReadState {outstanding_snapshots, ..}|
            outstanding_snapshots > 0).unwrap_or(false)
    }

    fn finished_until_snapshot(&self) -> bool {
        self.last_returned_to_client == self.last_snapshot
            && !self.has_outstanding_snapshots()
    }

    fn is_finished(&self) -> bool {
        assert!(!(self.outstanding_reads == 0
            && self.last_read_sent_to_server < self.last_snapshot),
            "outstanding_reads {:?}, last_read_sent_to_server {:?}, last_snapshot {:?}",
            self.outstanding_reads, self.last_read_sent_to_server, self.last_snapshot,
        );
        self.finished_until_snapshot()
            && !(self.is_searching_for_multi() || self.has_outstanding_snapshots())
    }
}

impl BufferCache {
    fn new() -> Self {
        BufferCache{
            vec_cache: VecDeque::new()
        }
    }

    fn alloc(&mut self) -> Vec<u8> {
        self.vec_cache.pop_front().unwrap_or(Vec::new())
    }

    fn cache_buffer(&mut self, mut buffer: Vec<u8>) {
        //TODO
        if self.vec_cache.len() < 100 {
            buffer.clear();
            self.vec_cache.push_front(buffer)
        }
    }
}

impl AsyncStoreClient for mpsc::Sender<Message> {
    fn on_finished_read(&mut self, read_loc: OrderIndex, read_packet: Vec<u8>) {
        let _ = self.send(Message::FromStore(ReadComplete(read_loc, read_packet)));
    }

    //TODO what info is needed?
    fn on_finished_write(&mut self, write_id: Uuid, write_locs: Vec<OrderIndex>) {
        let _ = self.send(Message::FromStore(WriteComplete(write_id, write_locs)));
    }
}