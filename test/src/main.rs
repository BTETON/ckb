use ckb_test::specs::*;
use ckb_test::{
    utils::node_log,
    worker::{Notify, Workers},
    Spec,
};
use ckb_types::core::ScriptHashType;
use ckb_util::Mutex;
use clap::{value_t, App, Arg};
use crossbeam_channel::unbounded;
use log::{error, info};
use rand::{seq::SliceRandom, thread_rng};
use std::any::Any;
use std::cmp::min;
use std::collections::HashMap;
use std::env;
use std::fs::{read_to_string, File};
use std::io::{BufRead, BufReader};
use std::panic;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[allow(clippy::cognitive_complexity)]
fn main() {
    env::set_var("RUST_BACKTRACE", "full");
    let _ = {
        let filter = ::std::env::var("CKB_LOG").unwrap_or_else(|_| "info".to_string());
        env_logger::builder().parse_filters(&filter).try_init()
    };

    let clap_app = clap_app();
    let matches = clap_app.get_matches();

    let binary = matches.value_of("binary").unwrap();
    let start_port = value_t!(matches, "port", u16).unwrap_or_else(|err| err.exit());
    let spec_names_to_run: Vec<_> = matches.values_of("specs").unwrap_or_default().collect();
    let max_time = if matches.is_present("max-time") {
        value_t!(matches, "max-time", u64).unwrap_or_else(|err| err.exit())
    } else {
        0
    };
    let worker_count = value_t!(matches, "concurrent", usize).unwrap_or_else(|err| err.exit());
    let vendor = value_t!(matches, "vendor", PathBuf).unwrap_or_else(|_| current_dir());

    if matches.is_present("list-specs") {
        list_specs();
        return;
    }

    info!("binary: {}", binary);
    info!("start port: {}", start_port);
    info!("max time: {:?}", max_time);

    let specs = filter_specs(all_specs(), spec_names_to_run);
    let total = specs.len();
    let worker_count = min(worker_count, total);
    let specs = Arc::new(Mutex::new(specs));
    let start_time = Instant::now();
    let mut spec_error: Option<Box<dyn Any + Send>> = None;
    let mut panicked_error = false;
    let mut error_spec_name = String::new();

    let (notify_tx, notify_rx) = unbounded();

    info!("start {} workers...", worker_count);
    let mut workers = Workers::new(
        worker_count,
        Arc::clone(&specs),
        notify_tx.clone(),
        start_port,
        binary.to_string(),
        vendor.clone(),
    );
    workers.start();

    let mut rerun_specs = Vec::new();
    let mut worker_running = worker_count;
    let mut done_specs = 0;
    while worker_running > 0 {
        if max_time > 0 && start_time.elapsed().as_secs() > max_time {
            // shutdown, specs running to long
            workers.shutdown();
        }

        let msg = match notify_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(msg) => msg,
            Err(err) => {
                if err.is_timeout() {
                    continue;
                }
                panic!(err);
            }
        };
        match msg {
            Notify::Error {
                spec_error: err,
                spec_name,
                node_dirs,
            } => {
                error_spec_name = spec_name.clone();
                rerun_specs.push(spec_name);
                workers.shutdown();
                worker_running -= 1;
                spec_error = Some(err);
                tail_node_logs(node_dirs);
            }
            Notify::Panick {
                spec_name,
                node_dirs,
            } => {
                error_spec_name = spec_name.clone();
                rerun_specs.push(spec_name);
                workers.shutdown();
                worker_running -= 1;
                panicked_error = true;
                print_panicked_logs(&node_dirs);
            }
            Notify::Done { spec_name, seconds } => {
                done_specs += 1;
                info!(
                    "{}/{} .............. Done {} in {} seconds",
                    done_specs, total, spec_name, seconds
                );
            }
            Notify::Stop => {
                worker_running -= 1;
            }
        }
    }
    // join all workers threads
    workers.join_all();

    if max_time > 0 && start_time.elapsed().as_secs() > max_time {
        error!(
            "Exit ckb-test, because total running time({} seconds) exceeds limit({} seconds)",
            start_time.elapsed().as_secs(),
            max_time
        );
    }

    info!("Total elapsed time: {:?}", start_time.elapsed());

    rerun_specs.extend(specs.lock().iter().map(|t| t.0.clone()));

    if rerun_specs.is_empty() {
        return;
    }

    if spec_error.is_some() || panicked_error {
        error!("ckb-failed on spec {}", error_spec_name);
        info!("You can rerun remaining specs using following command:");
    } else {
        info!("You can run the skipped specs using following command:");
    }

    info!(
        "{} --bin {} --port {} {}",
        canonicalize_path(env::args().nth(0).unwrap_or_else(|| "ckb-test".to_string())).display(),
        canonicalize_path(binary).display(),
        start_port,
        rerun_specs.join(" "),
    );

    if let Some(err) = spec_error {
        panic::resume_unwind(err);
    }
}

type SpecMap = HashMap<&'static str, Box<dyn Spec + Send>>;
type SpecTuple = (String, Box<dyn Spec + Send>);

fn clap_app() -> App<'static, 'static> {
    App::new("ckb-test")
        .arg(
            Arg::with_name("binary")
                .short("b")
                .long("bin")
                .takes_value(true)
                .value_name("PATH")
                .help("Path to ckb executable")
                .default_value("../target/release/ckb"),
        )
        .arg(
            Arg::with_name("port")
                .short("p")
                .long("port")
                .takes_value(true)
                .help("Starting port number used to start ckb nodes")
                .default_value("9000"),
        )
        .arg(
            Arg::with_name("max-time")
                .long("max-time")
                .takes_value(true)
                .value_name("SECONDS")
                .help("Exit when total running time exceeds this limit"),
        )
        .arg(Arg::with_name("list-specs").long("list-specs"))
        .arg(Arg::with_name("specs").multiple(true))
        .arg(
            Arg::with_name("concurrent")
                .short("c")
                .long("concurrent")
                .takes_value(true)
                .help("The number of specs can running concurrently")
                .default_value("4"),
        )
}

fn filter_specs(mut all_specs: SpecMap, spec_names_to_run: Vec<&str>) -> Vec<SpecTuple> {
    if spec_names_to_run.is_empty() {
        let mut specs: Vec<_> = all_specs
            .into_iter()
            .map(|(spec_name, spec)| (spec_name.to_string(), spec))
            .collect();
        specs.shuffle(&mut thread_rng());
        specs
    } else {
        let mut specs = Vec::with_capacity(spec_names_to_run.len());
        for spec_name in spec_names_to_run {
            specs.push((
                spec_name.to_string(),
                all_specs.remove(spec_name).unwrap_or_else(|| {
                    eprintln!("Unknown spec {}", spec_name);
                    std::process::exit(1);
                }),
            ));
        }
        specs
    }
}

fn current_dir() -> PathBuf {
    ::std::env::current_dir()
        .expect("can't get current_dir")
        .join("vendor")
}

fn canonicalize_path<P: AsRef<Path>>(path: P) -> PathBuf {
    path.as_ref()
        .canonicalize()
        .unwrap_or_else(|_| path.as_ref().to_path_buf())
}

fn all_specs() -> SpecMap {
    let specs: Vec<Box<dyn Spec + Send>> = vec![
        Box::new(BlockRelayBasic),
        Box::new(BlockSyncFromOne),
        Box::new(BlockSyncForks),
        Box::new(BlockSyncDuplicatedAndReconnect),
        Box::new(BlockSyncOrphanBlocks),
        Box::new(BlockSyncWithUncle),
        Box::new(BlockSyncNonAncestorBestBlocks),
        Box::new(RequestUnverifiedBlocks),
        Box::new(SyncTimeout),
        Box::new(GetBlocksTimeout),
        Box::new(ChainContainsInvalidBlock),
        Box::new(ForkContainsInvalidBlock),
        Box::new(ChainFork1),
        Box::new(ChainFork2),
        Box::new(ChainFork3),
        Box::new(ChainFork4),
        Box::new(ChainFork5),
        Box::new(ChainFork6),
        Box::new(ChainFork7),
        Box::new(LongForks),
        Box::new(ForksContainSameTransactions),
        Box::new(ForksContainSameUncle),
        Box::new(DepositDAO),
        // pick from https://github.com/nervosnetwork/ckb/pull/1626
        // TODO: add NervosDAO tests back when we have a way to build longer
        // chains in tests.
        // TODO: rebuild NervosDAO tests once 2 phase withdraw overflow is
        // adopted.
        // Box::new(WithdrawDAO),
        // Box::new(WithdrawAndDepositDAOWithinSameTx),
        // Box::new(WithdrawDAOWithNotMaturitySince),
        // Box::new(WithdrawDAOWithOverflowCapacity),
        // Box::new(WithdrawDAOWithInvalidWitness),
        // Box::new(DAOWithSatoshiCellOccupied),
        Box::new(SpendSatoshiCell::new()),
        Box::new(MiningBasic),
        Box::new(BootstrapCellbase),
        Box::new(TemplateSizeLimit),
        Box::new(PoolReconcile),
        Box::new(PoolResurrect),
        Box::new(TransactionRelayBasic),
        Box::new(TransactionRelayLowFeeRate),
        // FIXME: There is a probability of failure on low resouce CI server
        // Box::new(TransactionRelayMultiple),
        Box::new(RelayInvalidTransaction),
        Box::new(TransactionRelayTimeout),
        Box::new(Discovery),
        Box::new(Disconnect),
        Box::new(MalformedMessage),
        Box::new(DepentTxInSameBlock),
        // TODO enable these after proposed/pending pool tip verfiry logic changing
        // Box::new(CellbaseMaturity),
        Box::new(ReferenceHeaderMaturity),
        Box::new(ValidSince),
        Box::new(SendLowFeeRateTx),
        Box::new(SendLargeCyclesTxInBlock::new()),
        Box::new(SendLargeCyclesTxToRelay::new()),
        Box::new(SendArrowTxs),
        Box::new(FeeEstimate),
        Box::new(DifferentTxsWithSameInput),
        Box::new(CompactBlockEmpty),
        Box::new(CompactBlockEmptyParentUnknown),
        Box::new(CompactBlockPrefilled),
        Box::new(CompactBlockMissingFreshTxs),
        Box::new(CompactBlockMissingNotFreshTxs),
        Box::new(CompactBlockLoseGetBlockTransactions),
        Box::new(CompactBlockRelayParentOfOrphanBlock),
        Box::new(CompactBlockRelayLessThenSharedBestKnown),
        Box::new(InvalidLocatorSize),
        Box::new(SizeLimit),
        Box::new(CyclesLimit),
        Box::new(SendSecpTxUseDepGroup::new(
            "send_secp_tx_use_dep_group_data_hash",
            ScriptHashType::Data,
        )),
        Box::new(SendSecpTxUseDepGroup::new(
            "send_secp_tx_use_dep_group_type_hash",
            ScriptHashType::Type,
        )),
        Box::new(SendMultiSigSecpTxUseDepGroup::new(
            "send_multisig_secp_tx_use_dep_group_data_hash",
            ScriptHashType::Data,
        )),
        Box::new(SendMultiSigSecpTxUseDepGroup::new(
            "send_multisig_secp_tx_use_dep_group_type_hash",
            ScriptHashType::Type,
        )),
        Box::new(CheckTypical2In2OutTx::default()),
        Box::new(AlertPropagation::default()),
        Box::new(IndexerBasic),
        Box::new(GenesisIssuedCells),
        // TODO These cases will fail occasionally because of some unknown
        // asynchronous issues.
        Box::new(IBDProcess),
        Box::new(WhitelistOnSessionLimit),
        // Box::new(IBDProcessWithWhiteList),
        Box::new(MalformedMessageWithWhitelist),
        Box::new(InsufficientReward),
        Box::new(UncleInheritFromForkBlock),
        Box::new(UncleInheritFromForkUncle),
        Box::new(PackUnclesIntoEpochStarting),
        Box::new(FeeOfTransaction),
        Box::new(FeeOfMaxBlockProposalsLimit),
        Box::new(FeeOfMultipleMaxBlockProposalsLimit),
        Box::new(ProposeButNotCommit),
        Box::new(ProposeDuplicated),
        Box::new(ForkedTransaction),
        Box::new(MissingUncleRequest),
        Box::new(HandlingDescendantsOfProposed),
        Box::new(HandlingDescendantsOfCommitted),
        Box::new(ProposeOutOfOrder),
        Box::new(SubmitTransactionWhenItsParentInGap),
        Box::new(SubmitTransactionWhenItsParentInProposed),
        Box::new(ProposeTransactionButParentNot),
        Box::new(ProposalExpireRuleForCommittingAndExpiredAtOneTime),
        Box::new(ReorgHandleProposals),
        Box::new(TransactionHashCollisionDifferentWitnessHashes),
        Box::new(DuplicatedTransaction),
        Box::new(ConflictInPending),
        Box::new(ConflictInGap),
        Box::new(ConflictInProposed),
        Box::new(DAOVerify),
    ];
    specs.into_iter().map(|spec| (spec.name(), spec)).collect()
}

fn list_specs() {
    let all_specs = all_specs();
    let mut names: Vec<_> = all_specs.keys().collect();
    names.sort();
    for spec_name in names {
        println!("{}", spec_name);
    }
}

// sed -n ${{panic_ln-300}},${{panic_ln+300}}p $node_log_path
fn print_panicked_logs(node_dirs: &[String]) {
    for (i, node_dir) in node_dirs.iter().enumerate() {
        let node_log = node_log(&node_dir);
        let log_reader =
            BufReader::new(File::open(&node_log).expect("failed to read node's log")).lines();
        let panic_ln = log_reader.enumerate().find(|(_ln, line)| {
            line.as_ref()
                .map(|line| line.contains("panicked at"))
                .unwrap_or(false)
        });
        if panic_ln.is_none() {
            continue;
        }

        let panic_ln = panic_ln.unwrap().0;
        let print_lns = 600;
        let from_ln = panic_ln.saturating_sub(print_lns / 2) + 1;
        println!(
            "\n************** (Node.{}) sed -n {},{}p {}",
            i,
            from_ln,
            from_ln + print_lns,
            node_log.display(),
        );
        BufReader::new(File::open(&node_log).expect("failed to read node's log"))
            .lines()
            .skip(from_ln)
            .take(print_lns)
            .for_each(|line| {
                if let Ok(line) = line {
                    println!("{}", line);
                }
            });
    }
}

// tail -n 2000 $node_log_path
fn tail_node_logs(node_dirs: Vec<String>) {
    let tail_n: usize = env::var("CKB_TEST_TAIL_N")
        .unwrap_or_default()
        .parse()
        .unwrap_or(2000);

    for (i, node_dir) in node_dirs.into_iter().enumerate() {
        let node_log = node_log(&node_dir);
        let content = read_to_string(&node_log).expect("failed to read node's log");
        let skip = content.lines().count().saturating_sub(tail_n);

        println!(
            "\n************** (Node.{}) tail -n {} {}",
            i,
            tail_n,
            node_log.display()
        );
        for log in content.lines().skip(skip) {
            println!("{}", log);
        }
    }
}
