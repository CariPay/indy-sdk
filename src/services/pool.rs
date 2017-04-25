extern crate libc;
extern crate rust_base58;
extern crate serde_json;

use self::libc::c_int;
use self::rust_base58::FromBase58;
use std::cell::RefCell;
use std::collections::HashMap;
use std::{fmt, fs, io, thread};
use std::fmt::Debug;
use std::io::{BufRead, Write};
use rustc_serialize::json;
use zmq;

use commands::{Command, CommandExecutor};
use commands::pool::PoolCommand;
use errors::pool::PoolError;
use utils::sequence::SequenceUtils;
use utils::environment::EnvironmentUtils;

pub struct PoolService {
    pools: RefCell<HashMap<i32, Pool>>,
}

struct Pool {
    name: String,
    id: i32,
    send_sock: zmq::Socket,
    worker: Option<thread::JoinHandle<()>>,
}

struct PoolWorker {
    cmd_sock: zmq::Socket,
    open_cmd_id: i32,
    pool_id: i32,
    nodes: Vec<RemoteNode>,
}

impl PoolWorker {
    fn connect_to_known_nodes(&mut self) {
        let f = fs::File::open("pool_transactions_sandbox").expect("open file"); //FIXME use file for the pool
        let reader = io::BufReader::new(&f);
        for line in reader.lines() {
            let line: String = line.expect("read transaction line");
            let mut rn: RemoteNode = RemoteNode::new(line.as_str());
            let ctx: zmq::Context = zmq::Context::new();
            rn.connect(&ctx);
            rn.zsock.as_ref().unwrap().send("pi".as_bytes(), 0).expect("send ping");
            self.nodes.push(rn);
        }
    }

    pub fn run(&mut self) {
        self.connect_to_known_nodes();
        let mut ss_to_poll: Vec<zmq::PollItem> = Vec::new();
        ss_to_poll.push(self.cmd_sock.as_poll_item(zmq::POLLIN));
        for ref node in &self.nodes {
            let s: &zmq::Socket = node.zsock.as_ref().unwrap();
            ss_to_poll.push(s.as_poll_item(zmq::POLLIN));
        }
        CommandExecutor::instance().send(Command::Pool(
            PoolCommand::OpenAck(self.open_cmd_id, Ok(self.pool_id)))).expect("send ack cmd"); //TODO send only after catch-up?
        loop {
            trace!("zmq poll loop >>");
            let r = zmq::poll(ss_to_poll.as_mut_slice(), -1).expect("poll");
            trace!("zmq poll loop ... {:?}", r);
            for i in 0..self.nodes.len() {
                if ss_to_poll[1 + i].is_readable() {
                    self.nodes[i].recv_msg().expect("recv msg");
                }
            }
            if ss_to_poll[0].is_readable() {
                let cmd = self.cmd_sock.recv_string(zmq::DONTWAIT);
                trace!("cmd {:?}", cmd);
                if cmd.is_ok() {
                    if "exit".eq(cmd.unwrap().expect("non-string command").as_str()) {
                        break;
                    }
                }
            }
            trace!("zmq poll loop <<");
        }
        info!("zmq poll loop finished");
    }
}

impl Pool {
    pub fn new(name: &str, cmd_id: i32) -> Result<Pool, PoolError> {
        let zmq_ctx = zmq::Context::new();
        let recv_cmd_sock = zmq_ctx.socket(zmq::SocketType::PAIR)?;
        let send_cmd_sock = zmq_ctx.socket(zmq::SocketType::PAIR)?;
        let inproc_sock_name: String = format!("inproc://pool_{}", name);

        recv_cmd_sock.bind(inproc_sock_name.as_str())?;

        send_cmd_sock.connect(inproc_sock_name.as_str())?;
        let pool_id = SequenceUtils::get_next_id();
        let mut pool_worker: PoolWorker = PoolWorker {
            cmd_sock: recv_cmd_sock,
            open_cmd_id: cmd_id,
            pool_id: pool_id,
            nodes: Vec::new(),
        };

        Ok(Pool {
            name: name.to_string(),
            id: pool_id,
            send_sock: send_cmd_sock,
            worker: Some(thread::spawn(move || {
                pool_worker.run();
            })),
        })
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        let target = format!("pool{}", self.name);
        info!(target: target.as_str(), "Drop started");
        self.send_sock.send("exit".as_bytes(), 0).expect("send exit command"); //TODO
        info!(target: target.as_str(), "Drop wait worker");
        // Option worker type and this kludge is workaround for rust
        self.worker.take().unwrap().join().unwrap();
        info!(target: target.as_str(), "Drop finished");
    }
}

#[derive(RustcDecodable, RustcEncodable)]
struct PoolConfig {
    genesis_txn: String
}

impl PoolConfig {
    fn default(name: &str) -> PoolConfig {
        let mut txn = name.to_string();
        txn += ".txn";
        PoolConfig { genesis_txn: txn }
    }
}

#[derive(Serialize, Deserialize, Debug)]
struct NodeData {
    alias: String,
    client_ip: String,
    client_port: u32,
    node_ip: String,
    node_port: u32,
    services: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug)]
struct GenTransaction {
    data: NodeData,
    dest: String,
    identifier: String,
    #[serde(rename = "txnId")]
    txn_id: String,
    #[serde(rename = "type")]
    txn_type: String,
}

struct RemoteNode {
    name: String,
    public_key: Vec<u8>,
    verify_key: Vec<u8>,
    zaddr: String,
    zsock: Option<zmq::Socket>,
}

impl Debug for RemoteNode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "RemoteNode: {{ public_key: {:?}, verify_key {:?}, zaddr {:?}, zsock is_some {} }}",
               self.public_key, self.verify_key, self.zaddr, self.zsock.is_some())
    }
}

impl RemoteNode {
    fn new(txn: &str) -> RemoteNode {
        let gen_tx: GenTransaction = serde_json::from_str(txn).expect("RemoteNode parsing");
        RemoteNode::from(gen_tx)
    }

    fn connect(&mut self, ctx: &zmq::Context) {
        let key_pair = zmq::CurveKeyPair::new().expect("create key pair");
        let s = ctx.socket(zmq::SocketType::DEALER).expect("socket for Node");
        s.set_curve_secretkey(key_pair.secret_key.as_str()).expect("set secret key");
        s.set_curve_publickey(key_pair.public_key.as_str()).expect("set public key");
        s.set_curve_serverkey(zmq::z85_encode(self.verify_key.as_slice()).unwrap().as_str()).expect("set verify key");
        s.connect(self.zaddr.as_str()).expect("connect to Node");
        self.zsock = Some(s);
    }

    fn recv_msg(&self) -> Result<Option<String>, PoolError> {
        impl From<Vec<u8>> for PoolError {
            fn from(_: Vec<u8>) -> Self {
                PoolError::Io(io::Error::from(io::ErrorKind::InvalidData))
            }
        }
        let msg: String = self.zsock.as_ref().unwrap().recv_string(zmq::DONTWAIT)??;
        info!(target: "RemoteNode_recv_msg", "{} {}", self.name, msg);

        match msg.as_ref() {
            "po" | "pi" => Ok(None), //TODO send pong, update last available?
            _ => Ok(Some(msg))
        }
    }
}

impl From<GenTransaction> for RemoteNode {
    fn from(tx: GenTransaction) -> RemoteNode {
        fn crypto_sign_ed25519_pk_to_curve25519(pk: &Vec<u8>) -> Vec<u8> {
            // TODO: fix hack:
            // this function isn't included to sodiumoxide rust wrappers,
            // temporary local binding is used to call libsodium-sys function
            extern {
                pub fn crypto_sign_ed25519_pk_to_curve25519(
                    curve25519_pk: *mut [u8; 32],
                    ed25519_pk: *const [u8; 32]) -> c_int;
            }
            let mut from: [u8; 32] = [0; 32];
            from.clone_from_slice(pk.as_slice());
            let mut to: [u8; 32] = [0; 32];
            unsafe {
                crypto_sign_ed25519_pk_to_curve25519(&mut to, &from);
            }
            to.iter().cloned().collect()
        }

        let public_key = tx.dest.as_str().from_base58().expect("dest field in GenTransaction isn't valid");
        RemoteNode {
            verify_key: crypto_sign_ed25519_pk_to_curve25519(&public_key),
            public_key: public_key,
            zaddr: format!("tcp://{}:{}", tx.data.client_ip, tx.data.client_port),
            zsock: None,
            name: tx.data.alias,
        }
    }
}

impl PoolService {
    pub fn new() -> PoolService {
        PoolService {
            pools: RefCell::new(HashMap::new()),
        }
    }

    pub fn create(&self, name: &str, config: Option<&str>) -> Result<(), PoolError> {
        let mut path = EnvironmentUtils::pool_path(name);
        let pool_config: PoolConfig = match config {
            Some(config) => json::decode(config)?,
            None => PoolConfig::default(name)
        };

        if path.as_path().exists() {
            return Err(PoolError::NotCreated("Already created".to_string()));
        }

        fs::create_dir_all(path.as_path())?;

        path.push(name);
        path.set_extension("txn");
        fs::copy(&pool_config.genesis_txn, path.as_path())?;
        path.pop();

        path.push("config");
        path.set_extension("json");
        let mut f: fs::File = fs::File::create(path.as_path())?;
        f.write(json::encode(&pool_config)?.as_bytes())?;
        f.flush()?;

        // TODO probably create another one file pool.json with pool description,
        // but now there is no info to save (except name witch equal to directory)

        Ok(())
    }

    pub fn delete(&self, name: &str) -> Result<(), PoolError> {
        unimplemented!()
    }

    pub fn open(&self, name: &str, config: Option<&str>) -> Result<i32, PoolError> {
        for pool in self.pools.borrow().values() {
            if name.eq(pool.name.as_str()) {
                //TODO change error
                return Err(PoolError::InvalidHandle("Already opened".to_string()));
            }
        }

        let cmd_id: i32 = SequenceUtils::get_next_id();
        let new_pool = Pool::new(name, cmd_id)?;
        //FIXME process config: check None (use default), transfer to Pool instance

        self.pools.borrow_mut().insert(new_pool.id, new_pool);
        return Ok(cmd_id);
    }

    pub fn close(&self, handle: i32) -> Result<(), PoolError> {
        unimplemented!()
    }

    pub fn refresh(&self, handle: i32) -> Result<(), PoolError> {
        unimplemented!()
    }

    pub fn get_pool_name(&self, handle: i32) -> Result<String, PoolError> {
        self.pools.borrow().get(&handle).map_or(
            Err(PoolError::InvalidHandle("Doesn't exists".to_string())),
            |pool: &Pool| Ok(pool.name.clone()))
    }
}

#[cfg(test)]
mod mocks {
    use super::*;

    use std::cell::RefCell;

    pub struct PoolService {
        create_results: RefCell<Vec<Result<(), PoolError>>>,
        delete_results: RefCell<Vec<Result<(), PoolError>>>,
        open_results: RefCell<Vec<Result<i32, PoolError>>>,
        close_results: RefCell<Vec<Result<(), PoolError>>>,
        refresh_results: RefCell<Vec<Result<(), PoolError>>>
    }

    impl PoolService {
        pub fn new() -> PoolService {
            PoolService {
                create_results: RefCell::new(Vec::new()),
                delete_results: RefCell::new(Vec::new()),
                open_results: RefCell::new(Vec::new()),
                close_results: RefCell::new(Vec::new()),
                refresh_results: RefCell::new(Vec::new())
            }
        }

        pub fn create(&self, name: &str, config: &str) -> Result<(), PoolError> {
            //self.create_results.pop().unwrap()
            unimplemented!()
        }

        pub fn delete(&self, name: &str) -> Result<(), PoolError> {
            //self.delete_results.pop().unwrap()
            unimplemented!()
        }

        pub fn open(&self, name: &str, config: &str) -> Result<i32, PoolError> {
            //self.open_results.pop().unwrap()
            unimplemented!()
        }

        pub fn close(&self, handle: i32) -> Result<(), PoolError> {
            //self.close_results.pop().unwrap()
            unimplemented!()
        }

        pub fn refresh(&self, handle: i32) -> Result<(), PoolError> {
            //self.refresh_results.pop().unwrap()
            unimplemented!()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_service_can_be_created() {
        let pool_service = PoolService::new();
        assert!(true, "No crashes on PoolService::new");
    }

    #[test]
    fn pool_service_can_be_dropped() {
        fn drop_test() {
            let pool_service = PoolService::new();
        }

        drop_test();
        assert!(true, "No crashes on PoolService::drop");
    }
}