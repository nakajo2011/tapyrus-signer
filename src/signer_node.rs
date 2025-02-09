// Copyright (c) 2019 Chaintope Inc.
// Distributed under the MIT software license, see the accompanying
// file COPYING or http://www.opensource.org/licenses/mit-license.php.

use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use bitcoin::{Address, PrivateKey, PublicKey};
use redis::ControlFlow;

use crate::blockdata::Block;
use crate::net::{ConnectionManager, Message, MessageType, Signature, SignerID};
use crate::rpc::{GetBlockchainInfoResult, TapyrusApi};
use crate::sign::sign;
use crate::timer::RoundTimeOutObserver;

/// Round interval.
pub static ROUND_INTERVAL_DEFAULT_SECS: u64 = 60;
/// Round time limit delta. Round timeout timer should be little longer than `ROUND_INTERVAL_DEFAULT_SECS`.
static ROUND_TIMELIMIT_DELTA: u64 = 5;

pub struct SignerNode<T: TapyrusApi, C: ConnectionManager> {
    connection_manager: C,
    params: NodeParameters<T>,
    current_state: NodeState,
    stop_signal: Option<Receiver<u32>>,
    master_index: usize,
    round_timer: RoundTimeOutObserver,
}

/// Signature HashMap type alias.
type SignatureMap = HashMap<SignerID, secp256k1::Signature>;

#[derive(Debug, Clone, PartialEq)]
pub enum NodeState {
    Joining,
    Master {
        signature_map: SignatureMap,
        candidate_block: Block,
    },
    Member,
}

fn sender_index(sender_id: &SignerID, pubkey_list: &[PublicKey]) -> usize {
    //Unknown sender is already ignored.
    pubkey_list
        .iter()
        .position(|pk| pk == &sender_id.pubkey)
        .unwrap()
}

impl<T: TapyrusApi, C: ConnectionManager> SignerNode<T, C> {
    pub fn new(connection_manager: C, params: NodeParameters<T>) -> Self
    where
        Self: Sized,
    {
        let timer_limit = params.round_duration + ROUND_TIMELIMIT_DELTA;
        SignerNode {
            connection_manager,
            params,
            current_state: NodeState::Joining,
            stop_signal: None,
            master_index: 0,
            round_timer: RoundTimeOutObserver::new(timer_limit),
        }
    }

    pub fn stop_handler(&mut self, receiver: Receiver<u32>) {
        self.stop_signal = Some(receiver);
    }

    pub fn start(&mut self) {
        if !self.params.skip_waiting_ibd {
            self.wait_for_ibd_finish(std::time::Duration::from_secs(10));
        }

        let (sender, receiver): (Sender<Message>, Receiver<Message>) = channel();
        let closure = move |message: Message| match sender.send(message) {
            Ok(_) => ControlFlow::Continue,
            Err(error) => {
                log::warn!("Happened error!: {:?}", error);
                ControlFlow::Break(())
            }
        };

        // redisとの通信を行うthreadを開始
        let _handler = self.connection_manager.start(closure);
        self.current_state = if self.params.master_flag {
            self.start_new_round()
        } else {
            NodeState::Member
        };
        log::info!(
            "node start. NodeState: {:?}, node_index: {}, master_index: {}",
            &self.current_state,
            &self.params.self_node_index,
            &self.master_index
        );

        // Roundのtimeoutを監視するthreadを開始
        self.round_timer.start().unwrap();
        // get error_handler that is for catch error within connection_manager.
        let connection_manager_error_handler = self.connection_manager.error_handler();
        loop {
            // After process when received message. Get message from receiver,
            // then change that state in main thread side.
            // messageを受け取った後の処理。receiverからmessageを受け取り、
            // stateの変更はmain thread側で行う。
            match &self.stop_signal {
                Some(ref r) => match r.try_recv() {
                    Ok(_) => {
                        log::warn!("Stop by Terminate Signal.");
                        self.round_timer.stop();
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                    Err(e) => {
                        panic!("{:?}", e);
                    }
                },
                None => {}
            }
            // Receiving message.
            match receiver.try_recv() {
                Ok(msg) => {
                    let next = self.process_message(msg);
                    self.current_state = next;
                }
                Err(_e) => {}
            }
            // Process for exceed time limit of Round.
            match self.round_timer.receiver.try_recv() {
                Ok(_) => {
                    // Round timeout. force round robin master node.
                    self.current_state = self.round_robin_master();
                    self.round_timer.restart().unwrap();
                }
                Err(_e) => {} // nothing to do.
            }
            // Should be panic, if happened error in connection_manager.
            match connection_manager_error_handler {
                Some(ref receiver) => match receiver.try_recv() {
                    Ok(e) => {
                        self.round_timer.stop();
                        panic!(e.to_string());
                    }
                    Err(_e) => {}
                },
                None => {
                    log::warn!("Failed to get error_handler of connection_manager!");
                }
            }
            // wait loop
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    /// Signer Node waits for connected Tapyrus Core Node complete IBD(Initial Block Download).
    fn wait_for_ibd_finish(&self, interval: Duration) {
        log::info!("Waiting finish Initial Block Download ...");
        log::info!("If you start right away, you can set `--skip-waiting-ibd` option. ");

        loop {
            match self
                .params
                .rpc
                .getblockchaininfo()
                .expect("RPC connection failed")
            {
                GetBlockchainInfoResult {
                    initialblockdownload: false,
                    ..
                } => {
                    break;
                }
                GetBlockchainInfoResult {
                    initialblockdownload: true,
                    blocks: height,
                    bestblockhash: hash,
                    ..
                } => {
                    log::info!("Waiting for finish Initial Block Download. Current block height: {}, current best hash: {}", height, hash);
                }
            }
            std::thread::sleep(interval);
        }
    }

    pub fn start_new_round(&self) -> NodeState {
        std::thread::sleep(Duration::from_secs(self.params.round_duration));

        let block = self.params.rpc.getnewblock(&self.params.address).unwrap();
        self.connection_manager.broadcast_message(Message {
            message_type: MessageType::Candidateblock(block.clone()),
            sender_id: self.params.signer_id,
        });

        let sig = sign(&self.params.private_key, &block.hash().unwrap());
        let mut signature_map: SignatureMap = HashMap::new();
        signature_map.insert(self.params.signer_id, sig);
        NodeState::Master {
            candidate_block: block,
            signature_map,
        }
    }

    pub fn process_message(&mut self, message: Message) -> NodeState {
        match message.message_type {
            MessageType::Candidateblock(block) => {
                self.process_candidateblock(&message.sender_id, &block)
            }
            MessageType::Signature(sig) => self.process_signature(&message.sender_id, &sig),
            MessageType::Completedblock(block) => {
                self.process_completedblock(&message.sender_id, &block)
            }
            MessageType::Roundfailure => self.process_roundfailure(&message.sender_id),
        }
    }

    fn process_candidateblock(&mut self, sender_id: &SignerID, block: &Block) -> NodeState {
        match self.current_state {
            NodeState::Member => {
                match self.params.rpc.testproposedblock(&block) {
                    Ok(_) => {
                        self.master_index = sender_index(sender_id, &self.params.pubkey_list);
                        let block_hash = block.hash().unwrap();
                        let sig = sign(&self.params.private_key, &block_hash);
                        self.connection_manager.broadcast_message(Message {
                            message_type: MessageType::Signature(crate::net::Signature(sig)),
                            sender_id: self.params.signer_id,
                        });
                        // TODO: Errorを処理する必要あるかな？
                        self.round_timer.restart().unwrap();
                    }
                    Err(_e) => {
                        log::warn!(
                            "Received Invalid candidate block!!: sender: {:?}",
                            sender_id
                        );
                    }
                }
            }
            _ => {}
        };

        self.current_state.clone()
    }

    fn block2message(&self, block: &Block) -> secp256k1::Message {
        secp256k1::Message::from_slice(block.hash().unwrap().borrow_inner()).unwrap()
    }
    fn verify_signature(
        &self,
        signature_map: &SignatureMap,
        block: &Block,
        sig: &secp256k1::Signature,
        sender_id: &SignerID,
    ) -> Result<(), crate::errors::Error> {
        if signature_map.contains_key(sender_id) {
            Err(crate::errors::Error::DuplicatedMessage)
        } else {
            let verifier = secp256k1::Secp256k1::verification_only();
            match verifier.verify(&self.block2message(block), sig, &sender_id.pubkey.key) {
                Err(e) => Err(crate::errors::Error::InvalidSignature(e)),
                Ok(_) => Ok(()),
            }
        }
    }
    fn process_signature(&mut self, sender_id: &SignerID, signature: &Signature) -> NodeState {
        match &self.current_state {
            NodeState::Master {
                signature_map: ref sig_map,
                candidate_block: ref block,
            } => {
                let mut signature_map: SignatureMap = sig_map.clone();
                match self.verify_signature(&signature_map, &block, &signature.0, &sender_id) {
                    Ok(_) => {
                        signature_map.insert(*sender_id, signature.0.clone());
                        if signature_map.len() as u8 >= self.params.threshold {
                            // call combineblocksigs
                            let sigs = signature_map.values().map(|v| *v).collect();
                            let completed_block =
                                self.params.rpc.combineblocksigs(&block, &sigs).unwrap();

                            // call submitblock
                            self.params.rpc.submitblock(&completed_block).unwrap();

                            // send completeblock message
                            let message = Message {
                                message_type: MessageType::Completedblock(completed_block),
                                sender_id: self.params.signer_id.clone(),
                            };
                            self.connection_manager.broadcast_message(message);

                            // start round robin.
                            self.round_robin_master()
                        } else {
                            NodeState::Master {
                                signature_map,
                                candidate_block: block.clone(),
                            }
                        }
                    }
                    Err(e) => {
                        log::warn!("Invalid Signature!: sender={:?}, error={:?}", &sender_id, e);
                        self.current_state.clone()
                    }
                }
            }
            state => state.clone(),
        }
    }

    /// Master role pass to the node of next index.
    fn round_robin_master(&mut self) -> NodeState {
        let next_index = (self.master_index + 1) % self.params.pubkey_list.len();
        self.master_index = next_index;
        let next_state = if self.params.self_node_index == next_index {
            // self node is master.
            self.start_new_round()
        } else {
            NodeState::Member
        };
        log::info!(
            "Round Robin: Next State {:?}, node_index: {}, master_inde: {}",
            next_state,
            self.params.self_node_index,
            self.master_index
        );
        next_state
    }
    fn process_completedblock(&mut self, sender_id: &SignerID, _block: &Block) -> NodeState {
        let index = sender_index(sender_id, &self.params.pubkey_list);
        if index == self.master_index {
            // authorization master.
            // start round robin of master node.
            return self.round_robin_master();
        }
        self.current_state.clone()
    }

    fn process_roundfailure(&self, _sender_id: &SignerID) -> NodeState {
        self.current_state.clone()
    }
}

pub struct NodeParameters<T: TapyrusApi> {
    pub pubkey_list: Vec<PublicKey>,
    pub threshold: u8,
    pub private_key: PrivateKey,
    pub rpc: std::sync::Arc<T>,
    pub address: Address,
    pub signer_id: SignerID,
    pub master_flag: bool,
    pub self_node_index: usize,
    pub round_duration: u64,
    pub skip_waiting_ibd: bool,
}

impl<T: TapyrusApi> NodeParameters<T> {
    pub fn new(
        pubkey_list: Vec<PublicKey>,
        private_key: PrivateKey,
        threshold: u8,
        rpc: T,
        master_flag: bool,
        round_duration: u64,
        skip_waiting_ibd: bool,
    ) -> NodeParameters<T> {
        let secp = secp256k1::Secp256k1::new();
        let self_pubkey = private_key.public_key(&secp);
        let address = Address::p2pkh(&self_pubkey, private_key.network);
        let signer_id = SignerID {
            pubkey: self_pubkey,
        };
        let master_flag = master_flag;

        let mut pubkey_list = pubkey_list;
        &pubkey_list.sort();
        let self_node_index = sender_index(&signer_id, &pubkey_list);
        NodeParameters {
            pubkey_list,
            threshold,
            private_key,
            rpc: Arc::new(rpc),
            address,
            signer_id,
            master_flag,
            self_node_index,
            round_duration,
            skip_waiting_ibd,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::thread::JoinHandle;
    use std::time::Duration;

    use redis::ControlFlow;

    use crate::net::{ConnectionManager, ConnectionManagerError, Message, Signature, SignerID};
    use crate::rpc::tests::{safety, safety_error, MockRpc, SafetyBlock};
    use crate::rpc::TapyrusApi;
    use crate::sign::sign;
    use crate::signer_node::{NodeParameters, NodeState, SignerNode};
    use crate::test_helper::{get_block, TestKeys};

    type SpyMethod = Box<dyn Fn(Arc<Message>) -> () + Send + 'static>;

    /// ConnectionManager for testing.
    pub struct TestConnectionManager {
        /// This is count of messages. TestConnectionManager waits for receiving the number of message.
        pub receive_count: u32,
        /// sender of message
        pub sender: Sender<Message>,
        /// receiver of message
        pub receiver: Receiver<Message>,
        /// A function which is called when the node try to broadcast messages.
        pub broadcast_assert: SpyMethod,
    }

    impl TestConnectionManager {
        pub fn new(receive_count: u32, broadcast_assert: SpyMethod) -> Self {
            let (sender, receiver): (Sender<Message>, Receiver<Message>) = channel();
            TestConnectionManager {
                receive_count,
                sender,
                receiver,
                broadcast_assert,
            }
        }
    }

    impl ConnectionManager for TestConnectionManager {
        type ERROR = crate::errors::Error;
        fn broadcast_message(&self, message: Message) {
            let rc_message = Arc::new(message);
            (self.broadcast_assert)(rc_message.clone());
        }

        fn start(
            &self,
            mut message_processor: impl FnMut(Message) -> ControlFlow<()> + Send + 'static,
        ) -> JoinHandle<()> {
            for _count in 0..self.receive_count {
                match self.receiver.recv() {
                    Ok(message) => {
                        log::debug!("Test message receiving!! {:?}", message.message_type);
                        message_processor(message);
                    }
                    Err(e) => log::warn!("happend receiver error: {:?}", e),
                }
            }
            thread::Builder::new()
                .name("TestConnectionManager start Thread".to_string())
                .spawn(|| {
                    thread::sleep(Duration::from_millis(300));
                })
                .unwrap()
        }

        fn error_handler(
            &mut self,
        ) -> Option<Receiver<ConnectionManagerError<crate::errors::Error>>> {
            None::<Receiver<ConnectionManagerError<crate::errors::Error>>>
        }
    }

    fn create_node<T: TapyrusApi>(
        current_state: NodeState,
        rpc: T,
    ) -> SignerNode<T, TestConnectionManager> {
        let closure: SpyMethod = Box::new(move |_message: Arc<Message>| {});
        let (node, _) = create_node_with_closure_and_publish_count(current_state, rpc, closure, 1);
        node
    }

    fn create_node_with_closure_and_publish_count<T: TapyrusApi>(
        current_state: NodeState,
        rpc: T,
        spy: SpyMethod,
        publish_count: u32,
    ) -> (SignerNode<T, TestConnectionManager>, Sender<Message>) {
        let testkeys = TestKeys::new();
        let pubkey_list = testkeys.pubkeys();
        let threshold = 3;
        let private_key = testkeys.key[0];

        let mut params =
            NodeParameters::new(pubkey_list, private_key, threshold, rpc, true, 0, true);
        params.round_duration = 0;
        let con = TestConnectionManager::new(publish_count, spy);
        let broadcaster = con.sender.clone();
        let mut node = SignerNode::new(con, params);
        node.current_state = current_state;
        (node, broadcaster)
    }

    /// Run node on other thread.
    pub fn setup_node(
        spy: SpyMethod,
        arc_block: SafetyBlock,
    ) -> (
        Arc<Mutex<SignerNode<MockRpc, TestConnectionManager>>>,
        Sender<u32>,
        Sender<Message>,
    ) {
        let testkeys = TestKeys::new();
        let pubkey_list = testkeys.pubkeys();
        let threshold = 2;
        let private_key = testkeys.key[0];

        let con = TestConnectionManager::new(1, spy);
        let rpc = MockRpc {
            return_block: arc_block.clone(),
        };
        let broadcaster = con.sender.clone();

        let (stop_signal, stop_handler): (Sender<u32>, Receiver<u32>) = channel();
        let mut params =
            NodeParameters::new(pubkey_list, private_key, threshold, rpc, false, 0, true);
        params.round_duration = 0;
        let arc_node = Arc::new(Mutex::new(SignerNode::new(con, params)));
        let node = arc_node.clone();
        let _handle = thread::Builder::new()
            .name("NodeMainThread".to_string())
            .spawn(move || {
                let mut node = node.lock().unwrap();
                node.stop_handler(stop_handler);
                node.start();
            })
            .unwrap();

        (arc_node, stop_signal, broadcaster)
    }

    pub fn get_initial_master_state() -> NodeState {
        let owner_private_key = TestKeys::new().key[0];
        let block = get_block(0);

        // Round master create signature itself, when broadcast candidate block. So,
        // signatures vector has one signature.
        let mut signature_map = HashMap::new();
        let signer_id = SignerID::new(owner_private_key.public_key(&secp256k1::Secp256k1::new()));
        signature_map.insert(signer_id, sign(&owner_private_key, &block.hash().unwrap()));
        NodeState::Master {
            candidate_block: block.clone(),
            signature_map,
        }
    }

    #[test]
    fn test_pubkey_list_sort() {
        use bitcoin::util::key::PublicKey;
        use std::str::FromStr;

        let testkeys = TestKeys::new();
        let pubkey_list = vec![
            PublicKey::from_str(
                "03831a69b8009833ab5b0326012eaf489bfea35a7321b1ca15b11d88131423fafc",
            )
            .unwrap(),
            PublicKey::from_str(
                "02ce7edc292d7b747fab2f23584bbafaffde5c8ff17cf689969614441e0527b900",
            )
            .unwrap(),
            PublicKey::from_str(
                "02785a891f323acd6cef0fc509bb14304410595914267c50467e51c87142acbb5e",
            )
            .unwrap(),
            PublicKey::from_str(
                "02d111519ba1f3013a7a613ecdcc17f4d53fbcb558b70404b5fb0c84ebb90a8d3c",
            )
            .unwrap(),
            PublicKey::from_str(
                "02472012cf49fca573ca1f63deafe59df842f0bbe77e9ac7e67b211bb074b72506",
            )
            .unwrap(),
        ];
        let threshold = 3;
        let private_key = testkeys.key[0];
        let params = NodeParameters::new(
            pubkey_list.clone(),
            private_key,
            threshold,
            MockRpc {
                return_block: safety_error("Not set block.".to_string()),
            },
            true,
            0,
            true,
        );

        assert_ne!(params.pubkey_list[0], pubkey_list[0]);
        assert_eq!(params.pubkey_list[1], pubkey_list[4]);
    }

    #[test]
    fn test_candidate_process() {
        let (broadcast_s, broadcast_r): (Sender<Arc<Message>>, Receiver<Arc<Message>>) = channel();
        let assertion = Box::new(move |message: Arc<Message>| {
            broadcast_s.send(message).unwrap();
        });
        let arc_block = safety(get_block(0));
        let (_node, stop_signal, broadcaster) = setup_node(assertion, arc_block);
        let message_str = r#"{"message_type": {"Candidateblock": [0, 0, 0, 32, 237, 101, 140, 196, 6, 112, 204, 237, 162, 59, 176, 182, 20, 130, 31, 230, 212, 138, 65, 209, 7, 209, 159, 63, 58, 86, 8, 173, 61, 72, 48, 146, 177, 81, 22, 10, 183, 17, 51, 180, 40, 225, 246, 46, 174, 181, 152, 174, 133, 143, 246, 96, 23, 201, 150, 1, 242, 144, 136, 183, 198, 74, 72, 29, 98, 132, 225, 69, 210, 155, 112, 191, 84, 57, 45, 41, 112, 16, 49, 210, 175, 159, 237, 95, 155, 178, 31, 187, 40, 79, 167, 28, 235, 35, 143, 105, 166, 212, 9, 93, 0, 1, 2, 0, 0, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 12, 0, 0, 0, 3, 92, 1, 1, 255, 255, 255, 255, 2, 0, 242, 5, 42, 1, 0, 0, 0, 25, 118, 169, 20, 207, 18, 219, 192, 75, 176, 222, 111, 182, 168, 122, 90, 235, 75, 46, 116, 201, 112, 6, 178, 136, 172, 0, 0, 0, 0, 0, 0, 0, 0, 38, 106, 36, 170, 33, 169, 237, 226, 246, 28, 63, 113, 209, 222, 253, 63, 169, 153, 223, 163, 105, 83, 117, 92, 105, 6, 137, 121, 153, 98, 180, 139, 235, 216, 54, 151, 78, 140, 249, 1, 32, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0] },"sender_id": [3, 131, 26, 105, 184, 0, 152, 51, 171, 91, 3, 38, 1, 46, 175, 72, 155, 254, 163, 90, 115, 33, 177, 202, 21, 177, 29, 136, 19, 20, 35, 250, 252]}"#;
        let message = serde_json::from_str::<Message>(message_str).unwrap();

        broadcaster.send(message).unwrap();
        let broadcast_message = broadcast_r.recv().unwrap();
        let actual = format!("{:?}", &broadcast_message.message_type);
        assert!(actual.starts_with("Signature(Signature"));
        stop_signal.send(1).unwrap(); // this line not necessary, but for manners.
    }

    #[test]
    fn test_candidate_process_invalid_block() {
        let (broadcast_s, broadcast_r): (Sender<Arc<Message>>, Receiver<Arc<Message>>) = channel();
        let spy = Box::new(move |message: Arc<Message>| {
            broadcast_s.send(message).unwrap();
        });
        let arc_block = safety_error("invalid block!".to_string());
        let (_node, stop_signal, bloadcaster) = setup_node(spy, arc_block);
        let message_str = r#"{"message_type": {"Candidateblock": [0, 0, 0, 32, 237, 101, 140, 196, 6, 112, 204, 237, 162, 59, 176, 182, 20, 130, 31, 230, 212, 138, 65, 209, 7, 209, 159, 63, 58, 86, 8, 173, 61, 72, 48, 146, 177, 81, 22, 10, 183, 17, 51, 180, 40, 225, 246, 46, 174, 181, 152, 174, 133, 143, 246, 96, 23, 201, 150, 1, 242, 144, 136, 183, 198, 74, 72, 29, 98, 132, 225, 69, 210, 155, 112, 191, 84, 57, 45, 41, 112, 16, 49, 210, 175, 159, 237, 95, 155, 178, 31, 187, 40, 79, 167, 28, 235, 35, 143, 105, 166, 212, 9, 93, 0, 1, 2, 0, 0, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 12, 0, 0, 0, 3, 92, 1, 1, 255, 255, 255, 255, 2, 0, 242, 5, 42, 1, 0, 0, 0, 25, 118, 169, 20, 207, 18, 219, 192, 75, 176, 222, 111, 182, 168, 122, 90, 235, 75, 46, 116, 201, 112, 6, 178, 136, 172, 0, 0, 0, 0, 0, 0, 0, 0, 38, 106, 36, 170, 33, 169, 237, 226, 246, 28, 63, 113, 209, 222, 253, 63, 169, 153, 223, 163, 105, 83, 117, 92, 105, 6, 137, 121, 153, 98, 180, 139, 235, 216, 54, 151, 78, 140, 249, 1, 32, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0] },"sender_id": [3, 131, 26, 105, 184, 0, 152, 51, 171, 91, 3, 38, 1, 46, 175, 72, 155, 254, 163, 90, 115, 33, 177, 202, 21, 177, 29, 136, 19, 20, 35, 250, 252]}"#;
        let message = serde_json::from_str::<Message>(message_str).unwrap();

        bloadcaster.send(message).unwrap();
        match broadcast_r.recv_timeout(Duration::from_millis(500)) {
            Ok(m) => panic!("Should not broadcast Signature message: {:?}", m),
            Err(_e) => assert!(true),
        }
        stop_signal.send(1).unwrap(); // this line not necessary, but for manners.
    }

    #[test]
    fn test_modify_master_index() {
        let initial_state = NodeState::Member;
        let arc_block = safety(get_block(0));
        let rpc = MockRpc {
            return_block: arc_block.clone(),
        };
        let mut node = create_node(initial_state, rpc);

        // pubkeys sorted index map;
        // 0 -> 4
        // 1 -> 0
        // 2 -> 3
        // 3 -> 2
        // 4 -> 1
        let sender_id = SignerID::new(TestKeys::new().pubkeys()[1]);
        assert_eq!(node.master_index, 0); // in begin, master_index is 0.
        let _next_state = node.process_candidateblock(&sender_id, &get_block(0));
        assert_eq!(node.master_index, 0);

        let sender_id = SignerID::new(TestKeys::new().pubkeys()[0]);
        let _next_state = node.process_candidateblock(&sender_id, &get_block(0));
        assert_eq!(node.master_index, 4);

        node.round_timer.stop();
    }

    #[test]
    fn test_timeout_roundrobin() {
        let closure: SpyMethod = Box::new(move |_message: Arc<Message>| {});
        let initial_state = NodeState::Member;
        let arc_block = safety(get_block(0));
        let rpc = MockRpc {
            return_block: arc_block.clone(),
        };
        let (mut node, _broadcaster) =
            create_node_with_closure_and_publish_count(initial_state, rpc, closure, 0);

        let (stop_signal, stop_handler): (Sender<u32>, Receiver<u32>) = channel();
        node.stop_handler(stop_handler);
        node.params.master_flag = false;

        assert_eq!(node.master_index, 0 as usize);
        let ss = stop_signal.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(6));
            ss.send(1).unwrap();
        });
        node.start();

        assert_eq!(node.master_index, 1 as usize);
    }

    /// 3 of 5 multisig
    /// Round owner will collect signatures.
    #[test]
    fn process_signature_test() {
        let block = get_block(0);
        let initial_state = get_initial_master_state();

        let arc_block = safety(get_block(0));
        let rpc = MockRpc {
            return_block: arc_block.clone(),
        };
        let mut node = create_node(initial_state, rpc);

        // sign node1
        {
            let private_key = TestKeys::new().key[1];
            let sender_id = SignerID::new(TestKeys::new().pubkeys()[1]);
            let block_hash = get_block(0).hash().unwrap();
            let sig = sign(&private_key, &block_hash);

            let next_state = node.process_signature(&sender_id, &Signature(sig));

            match next_state {
                NodeState::Master {
                    signature_map: ref sigs,
                    ..
                } => {
                    assert_eq!(sigs.len(), 2);
                }
                ref state => panic!("Should be Master node, but: {:?}", state),
            }

            node.current_state = next_state;
        }

        // sign node2
        // After node2 send signature, threshold is going to be met. So, signatures vector is
        // cleared and the block state object has is renewed.
        {
            {
                // guard context. lock is only enable in this block.
                let mut guard_block = arc_block.try_lock().unwrap();
                std::mem::replace(&mut *guard_block, Ok(get_block(1))).unwrap();
            }
            let private_key = TestKeys::new().key[2];
            let sender_id = SignerID::new(TestKeys::new().pubkeys()[2]);
            let block_hash = block.hash().unwrap();
            let sig = sign(&private_key, &block_hash);

            let next_state = node.process_signature(&sender_id, &Signature(sig));

            match next_state {
                NodeState::Member => assert!(true),
                n => panic!("should be Member, but: {:?}", n),
            }
        }
    }

    #[test]
    fn test_invalid_signature() {
        let initial_state = get_initial_master_state();

        let arc_block = safety(get_block(0));
        let rpc = MockRpc {
            return_block: arc_block.clone(),
        };
        let mut node = create_node(initial_state, rpc);

        // sign node1
        {
            let private_key = TestKeys::new().key[1];
            let sender_id = SignerID::new(TestKeys::new().pubkeys()[0]);
            let block_hash = get_block(0).hash().unwrap();
            let sig = sign(&private_key, &block_hash);
            let next_state = node.process_signature(&sender_id, &Signature(sig));

            match next_state {
                NodeState::Master {
                    signature_map: ref sigs,
                    ..
                } => {
                    assert_eq!(sigs.len(), 1); // invalid signature do not include.
                }
                ref state => panic!("{:?}", state),
            }

            node.current_state = next_state;
        }
    }

    #[test]
    fn test_ignore_duplicate_signature() {
        let initial_state = get_initial_master_state();

        let arc_block = safety(get_block(0));
        let rpc = MockRpc {
            return_block: arc_block.clone(),
        };
        let mut node = create_node(initial_state, rpc);

        // sign node1
        {
            let private_key = TestKeys::new().key[1];
            let sender_id = SignerID::new(TestKeys::new().pubkeys()[1]);
            let block_hash = get_block(0).hash().unwrap();
            let sig = sign(&private_key, &block_hash);

            node.process_signature(&sender_id, &Signature(sig));
            // duplicate message
            let next_state = node.process_signature(&sender_id, &Signature(sig));

            match next_state {
                NodeState::Master {
                    signature_map: ref sigs,
                    ..
                } => {
                    assert_eq!(sigs.len(), 2); // ignore duplicate.
                }
                ref state => panic!("{:?}", state),
            }

            node.current_state = next_state;
        }
    }

    #[test]
    fn test_process_completedblock() {
        let initial_state = NodeState::Member;
        let arc_block = safety(get_block(0));
        let rpc = MockRpc {
            return_block: arc_block.clone(),
        };
        let mut node = create_node(initial_state, rpc);

        // pubkeys sorted index map;
        // 0 -> 4
        // 1 -> 0
        // 2 -> 3
        // 3 -> 2
        // 4 -> 1
        let sender_id = SignerID::new(TestKeys::new().pubkeys()[1]);
        assert_eq!(node.master_index, 0); // in begin, master_index is 0.
        let next_state = node.process_completedblock(&sender_id, &get_block(0));
        assert_eq!(node.master_index, 1); // should incremented.
        match next_state {
            NodeState::Member => assert!(true),
            n => panic!("Should be Member, but state:{:?}", n),
        }

        node.master_index = 4;
        let sender_id = SignerID::new(TestKeys::new().pubkeys()[0]);
        let next_state = node.process_completedblock(&sender_id, &get_block(0));
        assert_eq!(node.master_index, 0); // wrap back to 0.
        match next_state {
            NodeState::Member => assert!(true),
            n => panic!("Should be Member, but state:{:?}", n),
        }

        node.master_index = 3;
        let sender_id = SignerID::new(TestKeys::new().pubkeys()[2]);
        let next_state = node.process_completedblock(&sender_id, &get_block(0));
        assert_eq!(node.master_index, 4); // wrap back to 0.
        match next_state {
            NodeState::Master { signature_map, .. } => {
                assert_eq!(signature_map.len(), 1); // has self signature at start.
            }
            n => panic!("Should be Master, but state:{:?}", n),
        }
    }

    #[test]
    fn test_process_completedblock_ignore_different_master() {
        let initial_state = NodeState::Member;
        let arc_block = safety(get_block(0));
        let rpc = MockRpc {
            return_block: arc_block.clone(),
        };
        let mut node = create_node(initial_state, rpc);

        // pubkeys sorted index map;
        // 0 -> 4
        // 1 -> 0
        // 2 -> 3
        // 3 -> 2
        // 4 -> 1
        let sender_id = SignerID::new(TestKeys::new().pubkeys()[0]);
        assert_eq!(node.master_index, 0); // in begin, master_index is 0.
        let next_state = node.process_completedblock(&sender_id, &get_block(0));
        assert_eq!(node.master_index, 0); // should not incremented if not recorded master.
        match next_state {
            NodeState::Member => assert!(true),
            n => panic!("Should be Member, but state:{:?}", n),
        }
    }

    mod test_for_waiting_ibd_finish {
        use crate::blockdata::Block;
        use crate::errors::Error;
        use crate::rpc::{GetBlockchainInfoResult, TapyrusApi};
        use crate::signer_node::tests::create_node;
        use crate::signer_node::NodeState;
        use bitcoin::Address;
        use secp256k1::Signature;
        use std::cell::Cell;

        struct MockRpc {
            pub results: [GetBlockchainInfoResult; 2],
            pub call_count: Cell<usize>,
        }

        impl TapyrusApi for MockRpc {
            fn getnewblock(&self, _address: &Address) -> Result<Block, Error> {
                unimplemented!()
            }
            fn testproposedblock(&self, _block: &Block) -> Result<(), Error> {
                unimplemented!()
            }
            fn combineblocksigs(
                &self,
                _block: &Block,
                _signatures: &Vec<Signature>,
            ) -> Result<Block, Error> {
                unimplemented!()
            }
            fn submitblock(&self, _block: &Block) -> Result<(), Error> {
                unimplemented!()
            }

            fn getblockchaininfo(&self) -> Result<GetBlockchainInfoResult, Error> {
                let result = self.results[self.call_count.get()].clone();

                self.call_count.set(self.call_count.get() + 1);

                Ok(result)
            }
        }

        #[test]
        fn test_wait_for_ibd_finish() {
            let json = serde_json::from_str("{\"chain\": \"test\", \"blocks\": 26826, \"headers\": 26826, \"bestblockhash\": \"7303687fb5d80781bd9fece466e76d97a94613d409d127030ff7f34081a899f7\", \"mediantime\": 1568103315, \"verificationprogress\": 1, \"initialblockdownload\": false, \"size_on_disk\": 11669126,  \"pruned\": false,  \"bip9_softforks\": {    \"csv\": {      \"status\": \"failed\",      \"startTime\": 1456790400, \"timeout\": 1493596800, \"since\": 2016 }, \"segwit\": { \"status\": \"failed\", \"startTime\": 1462060800, \"timeout\": 1493596800, \"since\": 2016 }},  \"warnings\": \"\"}").unwrap();
            let mut result1 = serde_json::from_value::<GetBlockchainInfoResult>(json).unwrap();
            result1.initialblockdownload = true;
            let mut result2 = result1.clone();
            result2.initialblockdownload = false;

            let rpc = MockRpc {
                results: [result1, result2],
                call_count: Cell::new(0),
            };

            let node = create_node(NodeState::Member, rpc);

            node.wait_for_ibd_finish(std::time::Duration::from_millis(1));

            let rpc = node.params.rpc.clone();
            assert_eq!(rpc.call_count.get(), 2);
        }
    }
}
