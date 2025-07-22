use shvproto::{List, RpcValue};
use shvrpc::{rpcmessage::{PeerId, RpcError, RpcErrorCode}, RpcFrame, RpcMessage, RpcMessageMetaTags};
use log::*;
use simple_logger::SimpleLogger;
use shvrpc::util::parse_log_verbosity;
use clap::{Args, Command, FromArgMatches, Parser};
use futures::{select, FutureExt};

use shvbroker::{brokerimpl::{next_peer_id, BrokerCommand, BrokerImpl, BrokerToPeerMessage, PeerKind}, config::{AccessConfig, AccessRule, BrokerConfig, Mount, Role, SharedBrokerConfig}, shvnode::{self, PUBLIC_DIR_LS_METHODS}, spawn::spawn_and_log_error};
use smol::channel::{self, Sender};

#[derive(Parser, Debug)]
struct CliOpts {
    /// Print application version and exit
    #[arg(long)]
    version: bool,
    /// Config file path
    #[arg(short, long)]
    config: Option<String>,
    /// Print current config to stdout
    #[arg(long)]
    print_config: bool,
    /// RW directory location, where access database will bee stored
    #[arg(short, long)]
    data_directory: Option<String>,
    /// Verbose mode (module, .)
    #[arg(short = 'v', long = "verbose")]
    verbose: Option<String>,
}

pub(crate) fn main() -> shvrpc::Result<()> {
    const SMOL_THREADS: &str = "SMOL_THREADS";
    if std::env::var(SMOL_THREADS).is_err_and(|e| matches!(e, std::env::VarError::NotPresent)) {
        if let Ok(num_threads) = std::thread::available_parallelism() {
            unsafe {
                std::env::set_var(SMOL_THREADS, num_threads.to_string());
            }
        }
    }
    let cli = Command::new("CLI");//.arg(arg!(-b - -built).action(clap::ArgAction::SetTrue));
    let cli = CliOpts::augment_args(cli);
    let cli_matches = cli.get_matches();
    let cli_opts = CliOpts::from_arg_matches(&cli_matches).map_err(|err| err.exit()).unwrap();

    if cli_opts.version {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let mut logger = SimpleLogger::new();
    logger = logger.with_level(LevelFilter::Info);
    if let Some(module_names) = cli_opts.verbose {
        for (module, level) in parse_log_verbosity(&module_names, module_path!()) {
            if let Some(module) = module {
                logger = logger.with_module_level(module, level);
            } else {
                logger = logger.with_level(level);
            }
        }
    }
    logger.init().unwrap();

    info!("=====================================================");
    info!("{} ver. {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
    info!("=====================================================");
    //trace!("trace message");
    //debug!("debug message");
    //info!("info message");
    //warn!("warn message");
    //error!("error message");
    //log!(target: "RpcMsg", Level::Debug, "RPC message");
    //log!(target: "Access", Level::Debug, "Access control message");

    let config = if let Some(config_file) = &cli_opts.config {
        info!("Loading config file {config_file}");
        match BrokerConfig::from_file(config_file) {
            Ok(config) => {config}
            Err(err) => {
                return Err(err);
            }
        }
    } else {
        info!("Using default config");
        let mut config = BrokerConfig::default();
        config.access.mounts.insert("brouk".to_string(), Mount{ mount_point: "brouk".to_string(), description: "".to_string() });
        config.access.roles.insert("brouk".to_string(), Role{ roles: vec![], access: vec![AccessRule{ shv_ri: "brouk/**:*".to_string(), grant: "su".to_string()}, AccessRule{ shv_ri: "brouk/**:*:*".to_string(), grant: "su".to_string()}] });
        config
    };
    let access = config.access.clone();
    if cli_opts.print_config {
        print_config(&config, &access)?;
        return Ok(());
    }
    info!("-----------------------------------------------------");
    let broker_impl = BrokerImpl::new(SharedBrokerConfig::new(config), access, None);
    let broker_sender = broker_impl.command_sender.clone();
    spawn_and_log_error(brouk_peer_loop(broker_sender.clone()));
    smol::block_on(shvbroker::brokerimpl::run_broker(broker_impl))
}

fn print_config(config: &BrokerConfig, access: &AccessConfig) -> shvrpc::Result<()> {
    // info!("Writing config to file: {file}");
    // if let Some(path) = Path::new(file).parent() {
    //     fs::create_dir_all(path)?;
    // }
    let mut config = config.clone();
    config.access = access.clone();
    println!("{}", &serde_yaml::to_string(&config)?);
    Ok(())
}

pub(crate) async fn brouk_peer_loop( broker_writer: Sender<BrokerCommand>, ) -> shvrpc::Result<()> {
    let peer_id = next_peer_id();
    debug!("Entering peer loop client ID: {peer_id}.");

    let (peer_writer, peer_reader) = channel::unbounded::<BrokerToPeerMessage>();

    info!("Client ID: {peer_id} login success.");
    broker_writer.send(
        BrokerCommand::NewPeer {
            peer_id,
            peer_kind: PeerKind::Device { user: "brouk".to_string(), device_id: Some("brouk".to_string()), mount_point: None },
            sender: peer_writer.clone()
        }).await?;

    let mut fut_receive_broker_message = Box::pin(peer_reader.recv()).fuse();
    loop {
        select! {
            broker_message = fut_receive_broker_message => match broker_message {
                Err(e) => {
                    debug!("Broker to Peer channel closed: {}", &e);
                    break;
                }
                Ok(msg) => {
                    match msg {
                        BrokerToPeerMessage::PasswordSha1(_) => {
                            panic!("PasswordSha1 cannot be received here")
                        }
                        BrokerToPeerMessage::DisconnectByBroker => {
                            info!("Disconnected by broker, client ID: {peer_id}");
                            break;
                        }
                        BrokerToPeerMessage::SendFrame(frame) => {
                            process_broker_to_peer_frame(peer_id, broker_writer.clone(), frame).await?;
                        }
                    }
                    fut_receive_broker_message = Box::pin(peer_reader.recv()).fuse();
                }
            }
        }
    }

    info!("Client ID: {peer_id} gone.");
    Ok(())
}

async fn process_broker_to_peer_frame(peer_id: PeerId, broker_sender: Sender<BrokerCommand>, frame: shvrpc::RpcFrame) -> shvrpc::Result<()> {
    debug!("Processing frame from broker: {frame:?}");
    if frame.is_request() {
        if let Ok(resp_meta) = RpcFrame::prepare_response_meta(&frame.meta) {
            let result = process_peer_request(frame).await;
            let mut msg = RpcMessage::from_meta(resp_meta);
            match result {
                Ok(result) => {
                    msg.set_result(result);
                }
                Err(err) => {
                    msg.set_error(RpcError{ code: RpcErrorCode::MethodCallException, message: err.to_string()});
                }
            }
            if let Ok(frame) = msg.to_frame() {
                broker_sender.send(BrokerCommand::FrameReceived { peer_id, frame }).await?;
            }
        }
    }
    Ok(())
}

async fn process_peer_request(frame: shvrpc::RpcFrame) -> Result<RpcValue, shvrpc::Error> {
    debug!("Processing frame from broker: {frame:?}");
    assert!(frame.is_request());
    let rpcmsg = frame.to_rpcmesage()?;
    let method = frame.method().ok_or_else(|| "Request without method".to_string())?;
    match method {
        "dir" => {
            let dir = shvnode::dir(PUBLIC_DIR_LS_METHODS.iter(), rpcmsg.param().into());
            Ok(dir)
        }
        "ls" => {
            Ok(List::default().into())
        }
        unknown_method => {
            Err(format!("Unknown method: {unknown_method}").into())
        }
    }
}
