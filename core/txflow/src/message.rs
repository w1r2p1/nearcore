use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use primitives::traits::WitnessSelectorLike;
use primitives::types;

pub type MessageRef<T> = Rc<RefCell<Message<T> >>;
pub type MessageWeakRef<T> = Weak<RefCell<Message<T>>>;

/// Epoch -> message.
pub type EpochMap<T> = HashMap<u64, MessageWeakRef<T>>;

/// Epoch -> (node uid -> message).
pub type EpochMapMap<T> = HashMap<u64, HashMap<u64, MessageWeakRef<T>>>;


// TODO: Consider using arena like in https://github.com/SimonSapin/rust-forest once TypedArena becomes stable.
/// Represents the message of the DAG, T is the payload parameter. For in-shard TxFlow and
/// beacon-chain TxFlow T takes different values.
#[derive(Debug)]
pub struct Message<T> {
    pub data: types::SignedMessageData<T>,

    self_ref: MessageWeakRef<T>,
    parents: Vec<MessageRef<T>>,

    // The following fields are computed based on the approved messages.
    /// The computed epoch of the message. If this message is restored from the epoch block then
    /// the epoch is taken from the data.
    pub computed_epoch: Option<u64>,
    /// Computed flag of whether this message is representative.
    computed_is_representative: Option<bool>,
    /// Computed flag of whether this message is a kickout.
    computed_is_kickout: Option<bool>,

    // The following are the approved messages, grouped by different criteria.
    /// Epoch -> messages that have that epoch, grouped as:
    /// owner_uid who created the message -> the message.
    approved_epochs: EpochMapMap<T>,
    /// Epoch -> a/any representative of that epoch (if there are several forked representative
    /// messages than any of them).
    approved_representatives: EpochMap<T>,
    /// Epoch -> a/any kickout message of a representative with epoch Epoch (if there are several
    /// forked kickout messages then any of them).
    approved_kickouts: EpochMap<T>,
    /// Epoch -> endorsements that endorse a/any (if there are several forked representative messages
    /// then any of them) representative message of that epoch, grouped as:
    /// owner_uid who created endorsement -> the endorsement message.
    approved_endorsements: EpochMapMap<T>,
    /// Epoch -> promises that approve a/any (if there are several forked kickout messages then any
    /// of them) kickout message (it by the def. has epoch Epoch+1) that kicks out representative
    /// message of epoch Epoch, grouped as:
    /// owner_uid who created the promise -> the promise message.
    approved_promises: EpochMapMap<T>,

    // Epoch that's being kicked out -> (kickout keyed on hash -> (owner_uid -> a/any promise from this owner)).
    //approved_promises: HashMap<u64, HashMap<MessageRef<T>, HashMap<u64, MessageRef<T>>>>,

    // NOTE, a single message can be simultaneously:
    // a) a representative message of epoch X;
    // b) an endorsement of a representative message of epoch Y, Y<X;
    // c) a promise for a kickout message of a representative message of epoch Z, Z<X (Z can be
    //    equal to Y).
    //
    // It can also be simultaneously:
    // a) a kickout message of a representative message of epoch A;
    // b) an endorsement of a representative message of epoch Y, Y<A;
    // c) a promise for a kickout message of a representative message of epoch Z, Z<A (Z can be
    //    equal to Y).
    //
    // * In both cases for (b) and (c) a message can give multiple endorsements and promises as long
    //   as Y and Z satisfy the constraints.
    // * Endorsements are explicit since they require a part of the BLS signature. Promises,
    //   kickouts, and representative messages are implied by the parent messages.
    // * A message cannot be both a representative message of epoch X and a kickout message of epoch
    //   X-1, because the former takes the precedence.
    // * Also a representative message is supposed to endorse itself which is done by an owner
    //   including the part of the BLS signature in it. If the signature is not included that it
    //   does not endorse itself and is considered to be a recoverable deviation from the protocol.
}

// Traits for the HashMap.
//impl<T> Hash for MessageRef<T> {
//    fn hash<H>(&self, state: &mut H)
//    where H: Hasher, {
//        state.write_u64(self.borrow().data.hash);
//        state.finish();
//    }
//}
//
//impl<T> PartialEq for MessageRef<T> {
//    fn eq(&self, other: &MessageRef<T>) -> bool {
//        self.borrow().data.hash == other.borrow().data.hash
//    }
//}
//
//impl<T> Eq for MessageRef<T> {}

/// Aggregate EpochMapMap's from parents.
macro_rules! aggregate_mapmaps {
    ( $self:ident, $( $field_name:ident ),+ ) => {
        for p in &$self.parents {
            $(
                for (epoch, per_epoch) in &p.borrow().$field_name {
                   let a = $self.$field_name.entry(*epoch)
                       .or_insert_with(|| HashMap::new());
                    a.extend(per_epoch.into_iter().map(|(k, v)| (k.clone(), v.clone())));
                }
            )+
        }
    };
}

/// Aggregate EpochMap's from parents.
macro_rules! aggregate_maps {
    ( $self:ident, $( $field_name:ident ),+ ) => {
        for p in &$self.parents {
            $(
                let p_field = &p.borrow().$field_name;
                $self.$field_name.extend(p_field.into_iter().map(|(k, v)| (k.clone(), v.clone())));
            )+
        }
    };
}


impl<T> Message<T> {
    pub fn new(data: types::SignedMessageData<T>) -> MessageRef<T> {
        let result = Rc::new(RefCell::new(
            Message {
                self_ref: Weak::new(),
                data,
                parents: vec![],
                computed_epoch: None,
                computed_is_representative: None,
                computed_is_kickout: None,
                approved_epochs: HashMap::new(),
                approved_representatives: HashMap::new(),
                approved_kickouts: HashMap::new(),
                approved_endorsements: HashMap::new(),
                approved_promises: HashMap::new(),
            }));
        // Keep weak reference to itself.
        result.borrow_mut().self_ref = Rc::downgrade(&result);
        result
    }

    pub fn link(parent: &MessageRef<T>, child: &MessageRef<T>) {
        child.borrow_mut().parents.push(Rc::clone(&parent));
    }

    pub fn unlink(parent: &MessageRef<T>, child: &MessageRef<T>) {
        child.borrow_mut().parents.retain(|ref x| !Rc::ptr_eq(&x, &parent));
    }

    /// Computes the aggregated data from the parents and updates the message.
    pub fn aggregate_parents(&mut self) {
        aggregate_mapmaps!(self, approved_epochs, approved_endorsements, approved_promises);
        aggregate_maps!(self, approved_representatives, approved_kickouts);
    }

    /// Determines the previous epoch of the current owner. Otherwise returns the starting_epoch.
    fn prev_epoch(&self, starting_epoch: u64) -> u64 {
        // Iterate over past messages that were created by the current owner and return their max
        // epoch. If such message not found then return starting_epoch.
        self.parents.iter().filter_map(|parent| {
            parent.borrow().approved_epochs.iter()
                .filter(|(_, epoch_messages)|
                    epoch_messages.contains_key(&self.data.body.owner_uid)
                ).map(|(epoch, _)| *epoch).max()
        }).max().unwrap_or(starting_epoch)
    }

    /// Determines whether the epoch of the current message should increase.
    fn should_promote<P>(&self, prev_epoch: u64, witness_selector: &P) -> bool
        where P : WitnessSelectorLike {
        let total_witnesses = witness_selector.epoch_witnesses(prev_epoch);
        let mut existing_witnesses = HashSet::new();
        for p in &self.parents {
            if let Some(s) = p.borrow().approved_epochs.get(&prev_epoch) {
                existing_witnesses.extend(s.into_iter().map(|(owner_uid, _)| *owner_uid));
                if existing_witnesses.len() > total_witnesses.len()*2/3 {
                    return true;
                }
            }
        }
        false
    }

    /// Determines whether this message is a representative message.
    /// The message is a representative of epoch X if this is the first message of the epoch's leader
    /// in the epoch X that satisfies either of the conditions:
    /// a) X = 0.
    /// b) It approves the representative message of the epoch X-1.
    /// c) It approves the kickout message for the epoch X-1 and it approves >2/3 promises for that
    ///    kickout message.
    fn is_representative<P: WitnessSelectorLike>(&self, witness_selector: &P) -> bool {
        let epoch = self.computed_epoch
            .expect("Message epoch should be computed before everything else.");
        let is_leader = witness_selector.epoch_leader(epoch) == self.data.body.owner_uid;

        if !is_leader {
            // If it is not a leader then don't bother.
            false
        } else if epoch == 0 {
            // Scenario (a).
            true
        } else if self.approved_representatives.contains_key(&(epoch-1)) {
            // Scenario (b).
            true
        } else {
            // Scenario (c).
            // TODO: Implement this scenario.
            false
        }
    }

    /// Determines whether this message is a kickout message.
    /// The message is a kickout message for epoch X-1 if this is the first message of the epoch's
    /// leader in the epoch X that does not approve the representative message of the epoch X-1.
    fn is_kickout(&self) -> bool {
        // TODO: Implement.
        false
    }

    /// Determines the list of kickouts for which this message gives a promise.
    /// The message is a promise for a kickout message for epoch X if it approves the kickout but
    /// does not approve the representative message for epoch X.
    fn promises_for_kickouts(&self) -> Vec<MessageRef<T>> {
        // TODO: Implement.
        vec![]
    }

    /// Determines the list of representatives endorsed by this message.
    /// The message is an endorsement for representative message of epoch X if it approves it, if
    /// it does not approve a promise by the same node for kickout of epoch X, and if it contains
    /// the corresponding BLS part.
    fn endorsements_for_representatives(&self) -> Vec<MessageRef<T>> {
        // TODO: Implement.
        vec![]
    }


    /// Computes epoch, is_representative, is_kickout using parents' information.
    /// If recompute_epoch = false then the epoch is not recomputed but taken from data.
    pub fn populate_from_parents<P>(&mut self,
                                    recompute_epoch: bool,
                                    starting_epoch: u64,
                                    witness_selector: &P) where P : WitnessSelectorLike {
        self.aggregate_parents();
        let owner_uid = self.data.body.owner_uid;

        // Compute epoch, if required.
        let epoch = if recompute_epoch {
            let prev_epoch = self.prev_epoch(starting_epoch);
            if self.should_promote(prev_epoch, witness_selector) {
                prev_epoch + 1 } else  {
                prev_epoch }

        } else {
            self.data.body.epoch
        };
        let self_ref = self.self_ref.clone();
        self.approved_epochs.entry(epoch).or_insert_with(|| map!{owner_uid => self_ref});
        self.computed_epoch = Some(epoch);

        // Compute if it is a representative.
        let self_ref = self.self_ref.clone();
        self.computed_is_representative = Some(self.is_representative(witness_selector));
        self.approved_representatives.entry(epoch).or_insert_with(|| self_ref);
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    pub struct FakePayload {

    }

    fn simple_message() -> MessageRef<FakePayload> {
        Message::new(types::SignedMessageData {
            owner_sig: 0,
            hash: 0,
            body: types::MessageDataBody {
                owner_uid: 0,
                parents: vec![],
                epoch: 0,
                payload: FakePayload {},
                endorsements: vec![],
            },
        })
    }

    #[test]
    fn two_nodes() {
        let a = simple_message();
        let b = simple_message();
        Message::link(&a, &b);
    }

    #[test]
    fn three_nodes() {
        let a = simple_message();
        let b = simple_message();
        let c = simple_message();
        Message::link(&a,&b);
        Message::link(&b,&c);
        Message::link(&a,&c);
    }

    #[test]
    fn unlink() {
        let a = simple_message();
        let b = simple_message();
        Message::link(&a, &b);
        Message::unlink(&a, &b);
        assert!(b.borrow().parents.is_empty());
    }

    #[test]
    fn test_mapmap_aggregation() {
        let pp1 = simple_message();
        let pp2 = simple_message();
        let p1 = simple_message();
        let p2 = simple_message();
        let c = simple_message();
        Message::link(&pp1,&p1);
        Message::link(&pp2,&p2);
        Message::link(&p1,&c);
        Message::link(&p2,&c);
        p1.borrow_mut().approved_epochs.insert(10, map!{0 => Rc::downgrade(&pp1)});
        p2.borrow_mut().approved_epochs.insert(11, map!{1 => Rc::downgrade(&pp2)});
        c.borrow_mut().aggregate_parents();
        let expected: HashMap<u64, HashMap<u64, MessageWeakRef<FakePayload>>> = map!{
        10 => map!{0 => Rc::downgrade(&pp1)},
        11 => map!{1 => Rc::downgrade(&pp2)} };
        let actual= &c.borrow().approved_epochs;
        assert_eq!(actual.len(), expected.len());
        for (epoch, epoch_content) in &expected {
            assert!(actual.contains_key(epoch));
            for (uid, message) in epoch_content {
               assert!(Rc::ptr_eq(&message.upgrade().unwrap(),
                                  &actual.get(&epoch).unwrap().get(uid).unwrap().upgrade().unwrap()));
            }
        }
    }

    struct FakeWitnessSelector {
       schedule: HashMap<u64, HashSet<u64>>,
    }
    impl FakeWitnessSelector {
       fn new() -> FakeWitnessSelector {
          FakeWitnessSelector {
              schedule: map!{
              0 => set!{0, 1, 2, 3}, 1 => set!{1, 2, 3, 4},
              9 => set!{0, 1, 2, 3}, 10 => set!{1, 2, 3, 4}}
          }
       }
    }

    impl WitnessSelectorLike for FakeWitnessSelector {
        fn epoch_witnesses(&self, epoch: u64) -> &HashSet<u64> {
            self.schedule.get(&epoch).unwrap()
        }
        fn epoch_leader(&self, epoch: u64) -> u64 {
            *self.epoch_witnesses(epoch).iter().min().unwrap()
        }
    }

    type FakeMessageRef = MessageRef<FakePayload>;

    fn simple_graph(epoch_a: u64, owner_a: u64,
                    epoch_b: u64, owner_b: u64,
                    epoch_c: u64, owner_c: u64,
                    epoch_d: u64, owner_d: u64,
                    starting_epoch: u64, owner_e: u64
    ) -> (FakeMessageRef, FakeMessageRef, FakeMessageRef, FakeMessageRef, FakeMessageRef) {
        let selector = FakeWitnessSelector::new();
        let a = simple_message();
        let b = simple_message();
        let c = simple_message();
        let d = simple_message();
        let e = simple_message();
        {
            let a_data = &mut a.borrow_mut().data;
            a_data.body.epoch = epoch_a;
            a_data.body.owner_uid = owner_a;
            let b_data = &mut b.borrow_mut().data;
            b_data.body.epoch = epoch_b;
            b_data.body.owner_uid = owner_b;
            let c_data = &mut c.borrow_mut().data;
            c_data.body.epoch = epoch_c;
            c_data.body.owner_uid = owner_c;
            let d_data = &mut d.borrow_mut().data;
            d_data.body.epoch = epoch_d;
            d_data.body.owner_uid = owner_d;
            let e_data = &mut e.borrow_mut().data;
            e_data.body.owner_uid = owner_e;
        }
        let starting_epoch = starting_epoch;
        for m in vec![&a, &b, &c, &d] {
            m.borrow_mut().populate_from_parents(false, starting_epoch,&selector);
            Message::link(m, &e);
        }

        e.borrow_mut().populate_from_parents(true, starting_epoch,&selector);
        return (a, b, c, d, e)
    }

    #[test]
    fn test_epoch_computation_no_promo() {
        let selector = FakeWitnessSelector::new();
        let starting_epoch = 9;
        // Corner case of having messages for epoch 10 without messages for epoch 9.
        let (_a, _b, _c, _d, e)
        = simple_graph(9, 0, 10, 1, 10, 2, 10, 3, starting_epoch, 0);

        assert_eq!(e.borrow().computed_epoch.unwrap(), 9);
    }

    #[test]
    fn test_epoch_computation_promo() {
        let selector = FakeWitnessSelector::new();
        let starting_epoch = 9;
        let (_a, _b, _c, _d, e)
        = simple_graph(9, 0, 9, 1, 9, 2, 10, 3, starting_epoch, 0);

        assert_eq!(e.borrow().computed_epoch.unwrap(), 10);
    }

    #[test]
    fn test_representatives_scenarios_a_b() {
        let selector = FakeWitnessSelector::new();
        let starting_epoch = 0;
        let (a, b, c, d, e)
        = simple_graph(0, 0,
                       0, 1,
                       0, 2,
                       1, 3,
                       starting_epoch, 1);

        assert!(a.borrow().computed_is_representative.unwrap());
        assert!(!b.borrow().computed_is_representative.unwrap());
        assert!(!c.borrow().computed_is_representative.unwrap());
        assert!(!d.borrow().computed_is_representative.unwrap());
        assert_eq!(e.borrow().computed_epoch.unwrap(), 1);
        assert!(e.borrow().computed_is_representative.unwrap());
    }
}