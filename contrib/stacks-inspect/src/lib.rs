// Copyright (C) 2025 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;
use std::{fs, io, process};

use clarity::types::chainstate::SortitionId;
use clarity::util::hash::{Sha512Trunc256Sum, to_hex};
use clarity::vm::analysis::effects_analyzer::EffectsAnalyzer;
use clarity::vm::analysis::run_analysis;
use clarity::vm::analysis::types::{
    AccountNonceAccess, AssetId, AssetOwnershipAccess, ChainStateRead, ContractAnalysis,
    ContractCall, ContractReference, EffectTarget, FunctionEffects, PrincipalReference, Purity,
    StorageLocation, TokenKind,
};
use clarity::vm::ast::build_ast;
use clarity::vm::clarity::ClarityConnection;
use clarity::vm::costs::LimitedCostTracker;
use clarity::vm::types::{PrincipalData, QualifiedContractIdentifier, Value};
use clarity::vm::{ClarityName, ClarityVersion};
use clarity_cli::read_file_or_stdin;
use regex::Regex;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Value as JsonValue, json};
use stacks_common::codec::StacksMessageCodec;
use stacks_common::types::chainstate::{BlockHeaderHash, StacksBlockId};
use stacks_common::types::sqlite::NO_PARAMS;
use stacks_common::util::hash::{Hash160, hex_bytes};
use stacks_common::util::vrf::VRFProof;
use stacks_common::{debug, info, warn};
use stackslib::burnchains::{Burnchain, Txid};
use stackslib::chainstate::burn::ConsensusHash;
use stackslib::chainstate::burn::db::sortdb::{
    SortitionDB, SortitionHandleContext, get_ancestor_sort_id,
};
use stackslib::chainstate::coordinator::OnChainRewardSetProvider;
use stackslib::chainstate::nakamoto::miner::{
    BlockMetadata, NakamotoBlockBuilder, NakamotoTenureInfo,
};
use stackslib::chainstate::nakamoto::{NakamotoBlock, NakamotoChainState};
use stackslib::chainstate::stacks::db::blocks::DummyEventDispatcher;
use stackslib::chainstate::stacks::db::{
    ChainStateBootData, ChainstateTx, StacksBlockHeaderTypes, StacksChainState, StacksHeaderInfo,
};
use stackslib::chainstate::stacks::miner::*;
use stackslib::chainstate::stacks::{Error as ChainstateError, *};
use stackslib::clarity_vm::clarity::{ClarityInstance, ClarityReadOnlyConnection};
use stackslib::clarity_vm::database::GetTenureStartId;
use stackslib::config::{Config, ConfigFile, DEFAULT_MAINNET_CONFIG};
use stackslib::core::*;
use stackslib::cost_estimates::UnitEstimator;
use stackslib::cost_estimates::metrics::UnitMetric;
use stackslib::util_lib::db::IndexDBTx;

/// Options common to many `stacks-inspect` subcommands
/// Returned by `process_common_opts()`
#[derive(Debug, Default)]
pub struct CommonOpts {
    pub config: Option<Config>,
}

/// Process arguments common to many `stacks-inspect` subcommands and drain them from `argv`
///
/// Args:
///  - `argv`: Full CLI args `Vec`
///  - `start_at`: Position in args vec where to look for common options.
///    For example, if `start_at` is `1`, then look for these options **before** the subcommand:
///    ```console
///    stacks-inspect --config testnet.toml replay-block path/to/chainstate
///    ```
pub fn drain_common_opts(argv: &mut Vec<String>, start_at: usize) -> CommonOpts {
    let mut i = start_at;
    let mut opts = CommonOpts::default();
    while let Some(arg) = argv.get(i) {
        let (prefix, opt) = arg.split_at(2);
        if prefix != "--" {
            // No args left to take
            break;
        }
        // "Take" arg
        i += 1;
        match opt {
            "config" => {
                let path = &argv[i];
                i += 1;
                let config_file = ConfigFile::from_path(path).unwrap_or_else(|e| {
                    panic!("Failed to read '{path}' as stacks-node config: {e}")
                });
                let config = Config::from_config_file(config_file, false).unwrap_or_else(|e| {
                    panic!("Failed to convert config file into node config: {e}")
                });
                opts.config.replace(config);
            }
            "network" => {
                let network = &argv[i];
                i += 1;
                let config_file = match network.to_lowercase().as_str() {
                    "helium" => ConfigFile::helium(),
                    "mainnet" => ConfigFile::mainnet(),
                    "mocknet" => ConfigFile::mocknet(),
                    "xenon" => ConfigFile::xenon(),
                    other => {
                        eprintln!("Unknown network choice `{other}`");
                        process::exit(1);
                    }
                };
                let config = Config::from_config_file(config_file, false).unwrap_or_else(|e| {
                    panic!("Failed to convert config file into node config: {e}")
                });
                opts.config.replace(config);
            }
            _ => panic!("Unrecognized option: {opt}"),
        }
    }
    // Remove options processed
    argv.drain(start_at..i);
    opts
}

#[derive(Debug, Clone, Copy)]
enum BlockSource {
    Nakamoto,
    Epoch2,
}

#[derive(Clone)]
struct BlockScanEntry {
    index_block_hash: StacksBlockId,
    source: BlockSource,
}

enum BlockSelection {
    All,
    Prefix(String),
    Last(u64),
    HeightRange { start: u64, end: u64 },
    IndexRange { start: u64, end: u64 },
    NakaIndexRange { start: u64, end: u64 },
    IndexRangeInfo,
    NakaIndexRangeInfo,
}

impl BlockSelection {
    fn clause(&self) -> String {
        match self {
            BlockSelection::All => "WHERE orphaned = 0 ORDER BY height ASC".into(),
            BlockSelection::Prefix(prefix) => format!(
                "WHERE orphaned = 0 AND index_block_hash LIKE '{prefix}%' ORDER BY height ASC",
            ),
            BlockSelection::Last(count) => {
                format!("WHERE orphaned = 0 ORDER BY height DESC LIMIT {count}")
            }
            BlockSelection::HeightRange { start, end } => format!(
                "WHERE orphaned = 0 AND height BETWEEN {start} AND {} ORDER BY height ASC",
                end.saturating_sub(1)
            ),
            BlockSelection::IndexRange { start, end } => {
                let blocks = end.saturating_sub(*start);
                format!("WHERE orphaned = 0 ORDER BY index_block_hash ASC LIMIT {start}, {blocks}")
            }
            BlockSelection::NakaIndexRange { start, end } => {
                let blocks = end.saturating_sub(*start);
                format!("WHERE orphaned = 0 ORDER BY index_block_hash ASC LIMIT {start}, {blocks}")
            }
            BlockSelection::IndexRangeInfo | BlockSelection::NakaIndexRangeInfo => {
                unreachable!("Info selections should not generate SQL clauses")
            }
        }
    }
}

fn parse_block_selection(mode: Option<&str>, argv: &[String]) -> Result<BlockSelection, String> {
    match mode {
        Some("prefix") => {
            let prefix = argv
                .get(3)
                .ok_or_else(|| "Missing <index-block-hash-prefix>".to_string())?
                .clone();
            Ok(BlockSelection::Prefix(prefix))
        }
        Some("last") => {
            let count = argv
                .get(3)
                .ok_or_else(|| "Missing <block-count>".to_string())?
                .parse::<u64>()
                .map_err(|_| "<block-count> must be a u64".to_string())?;
            Ok(BlockSelection::Last(count))
        }
        Some("range") => {
            let start = argv
                .get(3)
                .ok_or_else(|| "Missing <start-block>".to_string())?
                .parse::<u64>()
                .map_err(|_| "<start-block> must be a u64".to_string())?;
            let end = argv
                .get(4)
                .ok_or_else(|| "Missing <end-block>".to_string())?
                .parse::<u64>()
                .map_err(|_| "<end-block> must be a u64".to_string())?;
            if start >= end {
                return Err("<start-block> must be < <end-block>".into());
            }
            Ok(BlockSelection::HeightRange { start, end })
        }
        Some("index-range") => match argv.get(3) {
            None => Ok(BlockSelection::IndexRangeInfo),
            Some(start_arg) => {
                let start = start_arg
                    .parse::<u64>()
                    .map_err(|_| "<start-block> must be a u64".to_string())?;
                let end = argv
                    .get(4)
                    .ok_or_else(|| "Missing <end-block>".to_string())?
                    .parse::<u64>()
                    .map_err(|_| "<end-block> must be a u64".to_string())?;
                if start >= end {
                    return Err("<start-block> must be < <end-block>".into());
                }
                Ok(BlockSelection::IndexRange { start, end })
            }
        },
        Some("naka-index-range") => match argv.get(3) {
            None => Ok(BlockSelection::NakaIndexRangeInfo),
            Some(start_arg) => {
                let start = start_arg
                    .parse::<u64>()
                    .map_err(|_| "<start-block> must be a u64".to_string())?;
                let end = argv
                    .get(4)
                    .ok_or_else(|| "Missing <end-block>".to_string())?
                    .parse::<u64>()
                    .map_err(|_| "<end-block> must be a u64".to_string())?;
                if start >= end {
                    return Err("<start-block> must be < <end-block>".into());
                }
                Ok(BlockSelection::NakaIndexRange { start, end })
            }
        },
        Some(other) => Err(format!("Unrecognized option: {other}")),
        None => Ok(BlockSelection::All),
    }
}

fn collect_block_entries_for_selection(
    db_path: &str,
    selection: &BlockSelection,
    chainstate: &StacksChainState,
) -> Vec<BlockScanEntry> {
    let mut entries = Vec::new();
    let clause = selection.clause();

    match selection {
        BlockSelection::Last(limit) => {
            if collect_nakamoto_entries(&mut entries, &clause, chainstate, Some(*limit)) {
                return entries;
            }
            collect_epoch2_entries(&mut entries, &clause, db_path, Some(*limit));
        }
        BlockSelection::IndexRange { .. } => {
            collect_epoch2_entries(&mut entries, &clause, db_path, None);
        }
        BlockSelection::NakaIndexRange { .. } => {
            collect_nakamoto_entries(&mut entries, &clause, chainstate, None);
        }
        _ => {
            collect_epoch2_entries(&mut entries, &clause, db_path, None);
            collect_nakamoto_entries(&mut entries, &clause, chainstate, None);
        }
    }

    entries
}

fn limit_reached(limit: Option<u64>, current: usize) -> bool {
    limit.is_some_and(|max| current >= max as usize)
}

fn count_epoch2_index_entries(db_path: &str) -> u64 {
    let staging_blocks_db_path = format!("{db_path}/chainstate/vm/index.sqlite");
    let conn =
        Connection::open_with_flags(&staging_blocks_db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap_or_else(|e| {
                panic!("Failed to open staging blocks DB at {staging_blocks_db_path}: {e}");
            });
    let sql = "SELECT COUNT(*) FROM staging_blocks WHERE orphaned = 0";
    let mut stmt = conn.prepare(sql).unwrap_or_else(|e| {
        panic!("Failed to prepare query over staging_blocks: {e}");
    });
    stmt.query_row(NO_PARAMS, |row| row.get::<_, u64>(0))
        .unwrap_or_else(|e| {
            panic!("Failed to count staging blocks: {e}");
        })
}

fn count_nakamoto_index_entries(chainstate: &StacksChainState) -> u64 {
    let sql = "SELECT COUNT(*) FROM nakamoto_staging_blocks WHERE orphaned = 0";
    let conn = chainstate.nakamoto_blocks_db();
    let mut stmt = conn.prepare(sql).unwrap_or_else(|e| {
        panic!("Failed to prepare query over nakamoto_staging_blocks: {e}");
    });
    stmt.query_row(NO_PARAMS, |row| row.get::<_, u64>(0))
        .unwrap_or_else(|e| {
            panic!("Failed to count nakamoto staging blocks: {e}");
        })
}

fn collect_epoch2_entries(
    entries: &mut Vec<BlockScanEntry>,
    clause: &str,
    db_path: &str,
    limit: Option<u64>,
) -> bool {
    if limit_reached(limit, entries.len()) {
        return true;
    }

    let staging_blocks_db_path = format!("{db_path}/chainstate/vm/index.sqlite");
    let conn =
        Connection::open_with_flags(&staging_blocks_db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap_or_else(|e| {
                panic!("Failed to open staging blocks DB at {staging_blocks_db_path}: {e}");
            });
    let sql = format!("SELECT index_block_hash FROM staging_blocks {clause}");
    let mut stmt = conn.prepare(&sql).unwrap_or_else(|e| {
        panic!("Failed to prepare query over staging_blocks: {e}");
    });
    let mut rows = stmt.query(NO_PARAMS).unwrap_or_else(|e| {
        panic!("Failed to query staging_blocks: {e}");
    });
    while let Some(row) = rows.next().unwrap_or_else(|e| {
        panic!("Failed to read staging block row: {e}");
    }) {
        let index_block_hash: StacksBlockId = row.get(0).unwrap();
        entries.push(BlockScanEntry {
            index_block_hash,
            source: BlockSource::Epoch2,
        });

        if limit_reached(limit, entries.len()) {
            return true;
        }
    }

    false
}

fn collect_nakamoto_entries(
    entries: &mut Vec<BlockScanEntry>,
    clause: &str,
    chainstate: &StacksChainState,
    limit: Option<u64>,
) -> bool {
    if limit_reached(limit, entries.len()) {
        return true;
    }

    let sql = format!("SELECT index_block_hash FROM nakamoto_staging_blocks {clause}");
    let conn = chainstate.nakamoto_blocks_db();
    let mut stmt = conn.prepare(&sql).unwrap_or_else(|e| {
        panic!("Failed to prepare query over nakamoto_staging_blocks: {e}");
    });
    let mut rows = stmt.query(NO_PARAMS).unwrap_or_else(|e| {
        panic!("Failed to query nakamoto_staging_blocks: {e}");
    });
    while let Some(row) = rows.next().unwrap_or_else(|e| {
        panic!("Failed to read Nakamoto staging block row: {e}");
    }) {
        let index_block_hash: StacksBlockId = row.get(0).unwrap();
        entries.push(BlockScanEntry {
            index_block_hash,
            source: BlockSource::Nakamoto,
        });

        if limit_reached(limit, entries.len()) {
            return true;
        }
    }

    false
}

/// Replay blocks from chainstate database
/// Terminates on error using `process::exit()`
///
/// Arguments:
///  - `argv`: Args in CLI format: `<command-name> [args...]`
pub fn command_validate_block(argv: &[String], conf: Option<&Config>) {
    let print_help_and_exit = || -> ! {
        let n = &argv[0];
        eprintln!("Usage:");
        eprintln!("  {n} <database-path>");
        eprintln!("  {n} <database-path> prefix <index-block-hash-prefix>");
        eprintln!("  {n} <database-path> index-range [<start-index> <end-index>]");
        eprintln!("  {n} <database-path> naka-index-range [<start-index> <end-index>]");
        eprintln!("  {n} <database-path> range <start-height> <end-height>");
        eprintln!("  {n} <database-path> <last> <block-count>");
        eprintln!("  {n} --early-exit ... # Exit on first error found");
        process::exit(1);
    };

    let start = Instant::now();
    let mut args = argv.to_vec();
    let early_exit = if let Some("--early-exit") = args.get(1).map(String::as_str) {
        args.remove(1);
        true
    } else {
        false
    };
    let db_path = args.get(1).unwrap_or_else(|| print_help_and_exit());
    let mode = args.get(2).map(String::as_str);
    let selection = parse_block_selection(mode, &args).unwrap_or_else(|err| {
        eprintln!("{err}");
        print_help_and_exit();
    });

    let conf = conf.unwrap_or(&DEFAULT_MAINNET_CONFIG);
    let chain_state_path = format!("{db_path}/chainstate/");
    let (chainstate, _) = StacksChainState::open(
        conf.is_mainnet(),
        conf.burnchain.chain_id,
        &chain_state_path,
        None,
    )
    .unwrap_or_else(|e| {
        eprintln!("Failed to open chainstate at {chain_state_path}: {e}");
        process::exit(1);
    });

    match &selection {
        BlockSelection::IndexRangeInfo => {
            let total = count_epoch2_index_entries(db_path);
            println!("Total available entries: {total}");
            return;
        }
        BlockSelection::NakaIndexRangeInfo => {
            let total = count_nakamoto_index_entries(&chainstate);
            println!("Total available entries: {total}");
            return;
        }
        _ => {}
    }

    let work_items = collect_block_entries_for_selection(db_path, &selection, &chainstate);
    drop(chainstate);
    if work_items.is_empty() {
        println!("No blocks matched the requested selection.");
        return;
    }
    let total_blocks = work_items.len();
    let mut completed = 0;
    let mut errors: Vec<(StacksBlockId, String)> = Vec::new();

    for entry in work_items {
        if let Err(e) = validate_entry(db_path, conf, &entry) {
            if early_exit {
                print!("\r");
                io::stdout().flush().ok();
                println!("Block {}: {e}", entry.index_block_hash);
                process::exit(1);
            }
            print!("\r");
            io::stdout().flush().ok();
            errors.push((entry.index_block_hash.clone(), e));
        }
        completed += 1;
        let pct = ((completed as f32 / total_blocks as f32) * 100.0).floor() as usize;
        print!("\rValidating: {:>3}% ({}/{})", pct, completed, total_blocks);
        io::stdout().flush().ok();
    }

    print!("\rValidating: 100% ({}/{})\n", total_blocks, total_blocks);

    if !errors.is_empty() {
        println!(
            "\nValidation completed with {} error(s) found in {}s:",
            errors.len(),
            start.elapsed().as_secs()
        );
        for (hash, message) in errors.iter() {
            println!("  Block {hash}: {message}");
        }
        process::exit(1);
    }
    println!(
        "\nFinished validating {} blocks in {}s",
        total_blocks,
        start.elapsed().as_secs()
    );
}

fn validate_entry(db_path: &str, conf: &Config, entry: &BlockScanEntry) -> Result<(), String> {
    match entry.source {
        BlockSource::Nakamoto => replay_naka_staging_block(db_path, &entry.index_block_hash, conf),
        BlockSource::Epoch2 => replay_staging_block(db_path, &entry.index_block_hash, conf),
    }
}

/// Replay mock mined blocks from JSON files
/// Terminates on error using `process::exit()`
///
/// Arguments:
///  - `argv`: Args in CLI format: `<command-name> [args...]`
///  - `conf`: Optional config for running on non-mainnet chainstate
pub fn command_replay_mock_mining(argv: &[String], conf: Option<&Config>) {
    let print_help_and_exit = || -> ! {
        let n = &argv[0];
        eprintln!("Usage:");
        eprintln!("  {n} <database-path> <mock-mined-blocks-path>");
        process::exit(1);
    };

    // Process CLI args
    let db_path = argv.get(1).unwrap_or_else(|| print_help_and_exit());

    let blocks_path = argv
        .get(2)
        .map(PathBuf::from)
        .map(fs::canonicalize)
        .transpose()
        .unwrap_or_else(|e| panic!("Not a valid path: {e}"))
        .unwrap_or_else(|| print_help_and_exit());

    // Validate directory path
    if !blocks_path.is_dir() {
        panic!("{blocks_path:?} is not a valid directory");
    }

    // Read entries in directory
    let dir_entries = blocks_path
        .read_dir()
        .unwrap_or_else(|e| panic!("Failed to read {blocks_path:?}: {e}"))
        .filter_map(|e| e.ok());

    // Get filenames, filtering out anything that isn't a regular file
    let filenames = dir_entries.filter_map(|e| match e.file_type() {
        Ok(t) if t.is_file() => e.file_name().into_string().ok(),
        _ => None,
    });

    // Get vec of (block_height, filename), to prepare for sorting
    //
    // NOTE: Trusting the filename is not ideal. We could sort on data read from the file,
    // but that requires reading all files
    let re = Regex::new(r"^([0-9]+)\.json$").unwrap();
    let mut indexed_files = filenames
        .filter_map(|filename| {
            // Use regex to extract block number from filename
            let Some(cap) = re.captures(&filename) else {
                debug!("Regex capture failed on {filename}");
                return None;
            };
            // cap.get(0) return entire filename
            // cap.get(1) return block number
            let i = 1;
            let Some(m) = cap.get(i) else {
                debug!("cap.get({i}) failed on {filename} match");
                return None;
            };
            let Ok(bh) = m.as_str().parse::<u64>() else {
                debug!("parse::<u64>() failed on '{}'", m.as_str());
                return None;
            };
            Some((bh, filename))
        })
        .collect::<Vec<_>>();

    // Sort by block height
    indexed_files.sort_by_key(|(bh, _)| *bh);

    if indexed_files.is_empty() {
        panic!("No block files found in {blocks_path:?}");
    }

    info!(
        "Replaying {} blocks starting at {}",
        indexed_files.len(),
        indexed_files[0].0
    );

    for (bh, filename) in indexed_files {
        let filepath = blocks_path.join(filename);
        let block = AssembledAnchorBlock::deserialize_from_file(&filepath)
            .unwrap_or_else(|e| panic!("Error reading block {bh} from file: {e}"));
        info!("Replaying block from {filepath:?}";
            "block_height" => bh,
            "block" => ?block
        );
        replay_mock_mined_block(db_path, block, conf);
    }
}

/// Replay mock mined blocks from JSON files
/// Terminates on error using `process::exit()`
///
/// Arguments:
///  - `argv`: Args in CLI format: `<command-name> [args...]`
///  - `conf`: Optional config for running on non-mainnet chainstate
pub fn command_try_mine(argv: &[String], conf: Option<&Config>) {
    let print_help_and_exit = || {
        let n = &argv[0];
        eprintln!("Usage: {n} <working-dir> [min-fee [max-time]]");
        eprintln!();
        eprintln!(
            "Given a <working-dir>, try to ''mine'' an anchored block. This invokes the miner block"
        );
        eprintln!(
            "assembly, but does not attempt to broadcast a block commit. This is useful for determining"
        );
        eprintln!("what transactions a given chain state would include in an anchor block,");
        eprintln!("or otherwise simulating a miner.");
        process::exit(1);
    };

    // Parse subcommand-specific args
    let db_path = argv.get(1).unwrap_or_else(print_help_and_exit);
    let min_fee = argv
        .get(2)
        .map(|arg| arg.parse().expect("Could not parse min_fee"))
        .unwrap_or(u64::MAX);
    let max_time = argv
        .get(3)
        .map(|arg| arg.parse().expect("Could not parse max_time"))
        .unwrap_or(u64::MAX);

    let start = Instant::now();

    let conf = conf.unwrap_or(&DEFAULT_MAINNET_CONFIG);

    let burnchain_path = format!("{db_path}/burnchain");
    let sort_db_path = format!("{db_path}/burnchain/sortition");
    let chain_state_path = format!("{db_path}/chainstate/");

    let burnchain = conf.get_burnchain();
    let sort_db = SortitionDB::open(&sort_db_path, false, burnchain.pox_constants.clone())
        .unwrap_or_else(|e| panic!("Failed to open {sort_db_path}: {e}"));
    let (chainstate, _) = StacksChainState::open(
        conf.is_mainnet(),
        conf.burnchain.chain_id,
        &chain_state_path,
        None,
    )
    .unwrap_or_else(|e| panic!("Failed to open stacks chain state: {e}"));
    let chain_tip = SortitionDB::get_canonical_burn_chain_tip(sort_db.conn())
        .unwrap_or_else(|e| panic!("Failed to get sortition chain tip: {e}"));

    let estimator = Box::new(UnitEstimator);
    let metric = Box::new(UnitMetric);

    let mut mempool_db = MemPoolDB::open(
        conf.is_mainnet(),
        conf.burnchain.chain_id,
        &chain_state_path,
        estimator,
        metric,
    )
    .unwrap_or_else(|e| panic!("Failed to open mempool db: {e}"));

    // Parent Stacks header for block we are going to mine
    let parent_stacks_header =
        NakamotoChainState::get_canonical_block_header(chainstate.db(), &sort_db)
            .unwrap_or_else(|e| panic!("Error looking up chain tip: {e}"))
            .expect("No chain tip found");

    let burn_dbconn = sort_db.index_handle(&chain_tip.sortition_id);

    let mut settings = BlockBuilderSettings::limited();
    settings.max_miner_time_ms = max_time;

    let result = match &parent_stacks_header.anchored_header {
        StacksBlockHeaderTypes::Epoch2(..) => {
            let sk = StacksPrivateKey::random();
            let mut tx_auth = TransactionAuth::from_p2pkh(&sk).unwrap();
            tx_auth.set_origin_nonce(0);

            let mut coinbase_tx = StacksTransaction::new(
                TransactionVersion::Mainnet,
                tx_auth,
                TransactionPayload::Coinbase(CoinbasePayload([0u8; 32]), None, None),
            );

            coinbase_tx.chain_id = conf.burnchain.chain_id;
            coinbase_tx.anchor_mode = TransactionAnchorMode::OnChainOnly;
            let mut tx_signer = StacksTransactionSigner::new(&coinbase_tx);
            tx_signer.sign_origin(&sk).unwrap();
            let coinbase_tx = tx_signer.get_tx().unwrap();

            StacksBlockBuilder::build_anchored_block(
                &chainstate,
                &burn_dbconn,
                &mut mempool_db,
                &parent_stacks_header,
                chain_tip.total_burn,
                &VRFProof::empty(),
                &Hash160([0; 20]),
                &coinbase_tx,
                settings,
                None,
                &Burnchain::new(
                    &burnchain_path,
                    &burnchain.chain_name,
                    &burnchain.network_name,
                )
                .unwrap_or_else(|e| panic!("Failed to instantiate burnchain: {e}")),
            )
            .map(|(block, cost, size)| (block.block_hash(), block.txs, cost, size))
        }
        StacksBlockHeaderTypes::Nakamoto(..) => {
            NakamotoBlockBuilder::build_nakamoto_block(
                &chainstate,
                &burn_dbconn,
                &mut mempool_db,
                &parent_stacks_header,
                // tenure ID consensus hash of this block
                &parent_stacks_header.consensus_hash,
                // the burn so far on the burnchain (i.e. from the last burnchain block)
                chain_tip.total_burn,
                NakamotoTenureInfo::default(),
                settings,
                None,
                0,
                &[],
            )
            .map(
                |BlockMetadata {
                     block,
                     tenure_consumed,
                     tenure_size,
                     ..
                 }| {
                    (
                        block.header.block_hash(),
                        block.txs,
                        tenure_consumed,
                        tenure_size,
                    )
                },
            )
        }
    };

    let elapsed = start.elapsed();
    let summary = format!(
        "block @ height = {h} off of {pid} ({pch}/{pbh}) in {t}ms. Min-fee: {min_fee}, Max-time: {max_time}",
        h = parent_stacks_header.stacks_block_height + 1,
        pid = &parent_stacks_header.index_block_hash(),
        pch = &parent_stacks_header.consensus_hash,
        pbh = &parent_stacks_header.anchored_header.block_hash(),
        t = elapsed.as_millis(),
    );

    let code = match result {
        Ok((block_hash, txs, cost, size)) => {
            let total_fees: u64 = txs.iter().map(|tx| tx.get_tx_fee()).sum();

            println!("Successfully mined {summary}");
            println!("Block {block_hash}: {total_fees} uSTX, {size} bytes, cost {cost:?}");
            0
        }
        Err(e) => {
            println!("Failed to mine {summary}");
            println!("Error: {e}");
            1
        }
    };

    process::exit(code);
}

/// Compute the contract hash for a given contract
///
/// Arguments:
///  - `argv`: Args in CLI format: `<command-name> [args...]`
pub fn command_contract_hash(argv: &[String], _conf: Option<&Config>) {
    let print_help_and_exit = || -> ! {
        let n = &argv[0];
        eprintln!("Usage:");
        eprintln!("  {n} <CONTRACT_PATH | - (stdin)>");
        process::exit(1);
    };

    // Process CLI args
    let contract_path = argv.get(1).unwrap_or_else(|| print_help_and_exit());
    let contract_source = read_file_or_stdin(contract_path);

    let hash = Sha512Trunc256Sum::from_data(contract_source.as_bytes());
    let hex_string = to_hex(hash.as_bytes());
    let source_name = if contract_path == "-" {
        "stdin"
    } else {
        contract_path
    };
    println!("Contract hash for {source_name}:\n{hex_string}");
}

/// Analyze a contract in chainstate and print effect summaries.
///
/// Arguments:
///  - `argv`: Args in CLI format: `<command-name> [args...]`
pub fn command_contract_effects(argv: &[String], conf: Option<&Config>) {
    let print_help_and_exit = || -> ! {
        let n = &argv[0];
        eprintln!("Usage:");
        eprintln!("  {n} <database-path> <contract-identifier> [--json]");
        eprintln!();
        eprintln!("Example:");
        eprintln!("  {n} /tmp/chainstate SP3FBR2AGK8T3A1P84G0A1AD0H3Z2J9Y0T7H9VGRB.example --json");
        process::exit(1);
    };

    let mut args = argv.to_vec();
    let json_output = if let Some(pos) = args.iter().position(|arg| arg == "--json") {
        args.remove(pos);
        true
    } else {
        false
    };

    let db_path = args.get(1).unwrap_or_else(|| print_help_and_exit());
    let contract_str = args.get(2).unwrap_or_else(|| print_help_and_exit());
    let contract_id = match PrincipalData::parse_qualified_contract_principal(contract_str)
        .unwrap_or_else(|e| {
            eprintln!("Failed to parse contract identifier '{contract_str}': {e}");
            process::exit(1);
        }) {
        PrincipalData::Contract(contract_id) => contract_id,
        _ => {
            eprintln!("Expected a contract principal, got '{contract_str}'");
            process::exit(1);
        }
    };

    let conf = conf.unwrap_or(&DEFAULT_MAINNET_CONFIG);
    let data_root = db_path.to_string();
    let chain_state_path = format!("{data_root}/chainstate/");
    let sort_db_path = format!("{data_root}/burnchain/sortition");
    if !Path::new(&chain_state_path).exists() || !Path::new(&sort_db_path).exists() {
        eprintln!("Chainstate not found at {chain_state_path} (or sortition at {sort_db_path}).");
        process::exit(1);
    }

    let burnchain = conf.get_burnchain();
    let mut boot_data = ChainStateBootData::new(&burnchain, vec![], None);
    let (mut chainstate, _) = StacksChainState::open_and_exec(
        conf.is_mainnet(),
        conf.burnchain.chain_id,
        &chain_state_path,
        Some(&mut boot_data),
        None,
    )
    .unwrap_or_else(|e| panic!("Failed to open chainstate at {chain_state_path}: {e:?}"));
    let sort_db = SortitionDB::open(&sort_db_path, false, burnchain.pox_constants.clone())
        .unwrap_or_else(|e| panic!("Failed to open sortition DB at {sort_db_path}: {e:?}"));
    let chain_tip = SortitionDB::get_canonical_burn_chain_tip(sort_db.conn())
        .unwrap_or_else(|e| panic!("Failed to get sortition chain tip: {e}"));
    let burn_dbconn = sort_db.index_handle(&chain_tip.sortition_id);

    let parent_header = NakamotoChainState::get_canonical_block_header(chainstate.db(), &sort_db)
        .unwrap_or_else(|e| panic!("Failed to get chain tip: {e}"))
        .expect("No chain tip found");
    let stacks_tip = parent_header.index_block_hash();

    let analysis_result =
        chainstate.maybe_read_only_clarity_tx(&burn_dbconn, &stacks_tip, |clarity_tx| {
            let contract_source =
                clarity_tx.with_clarity_db_readonly(|db| db.get_contract_src(&contract_id));
            let Some(contract_source) = contract_source else {
                return Ok::<
                    Option<(ContractAnalysis, BTreeMap<ClarityName, FunctionEffects>)>,
                    String,
                >(None);
            };

            let clarity_version = clarity_tx
                .with_analysis_db_readonly(|db| db.get_clarity_version(&contract_id).ok())
                .unwrap_or_else(|| ClarityVersion::default_for_epoch(clarity_tx.get_epoch()));

            let epoch = clarity_tx.get_epoch();
            let mut cost_track = LimitedCostTracker::new_free();
            let contract_ast = build_ast(
                &contract_id,
                &contract_source,
                &mut cost_track,
                clarity_version,
                epoch,
            )
            .map_err(|e| format!("Failed to parse contract {contract_id}: {e}"))?;
            let analysis_result = clarity_tx.with_analysis_db_readonly(|db| {
                run_analysis(
                    &contract_id,
                    &contract_ast.expressions,
                    db,
                    false,
                    cost_track,
                    epoch,
                    clarity_version,
                    false,
                )
            });
            let mut analysis = analysis_result
                .map_err(|e| format!("Failed to analyze contract {contract_id}: {}", e.0))?;
            let raw_effects = analysis.function_effects.clone();
            let mut contracts = collect_contract_effects_recursive(
                clarity_tx,
                &contract_id,
                &analysis.function_effects,
            )?;
            resolve_contract_effects_transitively(&mut contracts);
            if let Some(resolved) = contracts.get(&contract_id) {
                analysis.function_effects = resolved.clone();
            }
            Ok(Some((analysis, raw_effects)))
        });

    let (analysis, raw_effects) = match analysis_result {
        Ok(Some(Ok(Some((analysis, raw_effects))))) => (analysis, raw_effects),
        Ok(Some(Ok(None))) => {
            eprintln!("Contract {contract_id} has no source in chainstate.");
            process::exit(1);
        }
        Ok(Some(Err(e))) => {
            eprintln!("Failed to analyze contract {contract_id}: {e}");
            process::exit(1);
        }
        Ok(None) => {
            eprintln!("Contract {contract_id} not found in chainstate.");
            process::exit(1);
        }
        Err(e) => {
            panic!("Failed to read contract from chainstate: {e:?}");
        }
    };

    if json_output {
        let mut effects_map = serde_json::Map::new();
        for (name, effects) in analysis.function_effects.iter() {
            effects_map.insert(name.to_string(), function_effects_to_json(effects));
        }
        let mut calls_map = serde_json::Map::new();
        for (name, effects) in raw_effects.iter() {
            let calls = effects
                .contract_calls
                .iter()
                .map(|call| {
                    json!({
                        "contract": format_contract_reference(&call.contract),
                        "function": call.function.to_string(),
                    })
                })
                .collect::<Vec<_>>();
            calls_map.insert(name.to_string(), JsonValue::Array(calls));
        }
        let value = json!({
            "contract": contract_id.to_string(),
            "clarity_version": format!("{:?}", analysis.clarity_version),
            "effects": effects_map,
            "calls": calls_map,
        });
        println!("{}", serde_json::to_string_pretty(&value).unwrap());
        return;
    }

    println!("Contract: {contract_id}");
    println!("Clarity version: {:?}", analysis.clarity_version);
    for (name, effects) in analysis.function_effects.iter() {
        println!();
        println!("Function {name} ({:?})", effects.purity);
        print_effects_section("reads", &effects.reads);
        print_effects_section("writes", &effects.writes);
        if !effects.contract_calls.is_empty() {
            println!("  contract-calls:");
            for call in effects.contract_calls.iter() {
                println!(
                    "    - {}.{}",
                    format_contract_reference(&call.contract),
                    call.function
                );
            }
        }
    }
}

/// Analyze a transaction from raw hex and print effect summaries.
///
/// Arguments:
///  - `argv`: Args in CLI format: `<command-name> [args...]`
pub fn command_tx_effects(argv: &[String], conf: Option<&Config>) {
    let print_help_and_exit = || -> ! {
        let n = &argv[0];
        eprintln!("Usage:");
        eprintln!("  {n} <database-path> <tx-hex|@file|-> [--json] [--graph]");
        eprintln!();
        eprintln!("Examples:");
        eprintln!("  {n} /tmp/chainstate 00deadbeef");
        eprintln!("  {n} /tmp/chainstate @/tmp/tx.hex --graph");
        process::exit(1);
    };

    let mut args = argv.to_vec();
    let json_output = if let Some(pos) = args.iter().position(|arg| arg == "--json") {
        args.remove(pos);
        true
    } else {
        false
    };
    let show_graph = if let Some(pos) = args.iter().position(|arg| arg == "--graph") {
        args.remove(pos);
        true
    } else {
        false
    };

    let db_path = args.get(1).unwrap_or_else(|| print_help_and_exit());
    let tx_arg = args.get(2).unwrap_or_else(|| print_help_and_exit());
    let tx_hex = read_hex_arg(tx_arg).unwrap_or_else(|e| {
        eprintln!("{e}");
        process::exit(1);
    });
    let tx = parse_tx_from_hex(&tx_hex).unwrap_or_else(|e| {
        eprintln!("{e}");
        process::exit(1);
    });

    let report =
        analyze_tx_effects_with_chainstate(db_path, &tx, conf, show_graph).unwrap_or_else(|e| {
            eprintln!("{e}");
            process::exit(1);
        });
    print_tx_effects_report(&report, json_output, show_graph);
}

/// Analyze a transaction by txid from chainstate and print effect summaries.
///
/// Arguments:
///  - `argv`: Args in CLI format: `<command-name> [args...]`
pub fn command_txid_effects(argv: &[String], conf: Option<&Config>) {
    let print_help_and_exit = || -> ! {
        let n = &argv[0];
        eprintln!("Usage:");
        eprintln!(
            "  {n} <database-path> <txid-hex> [--block-id <index-block-hash>] [--json] [--graph]"
        );
        process::exit(1);
    };

    let mut args = argv.to_vec();
    let block_id = if let Some(pos) = args.iter().position(|arg| arg == "--block-id") {
        let block_hex = args.get(pos + 1).unwrap_or_else(|| print_help_and_exit());
        let block_id = parse_block_id_hex(block_hex).unwrap_or_else(|e| {
            eprintln!("{e}");
            process::exit(1);
        });
        args.drain(pos..=pos + 1);
        Some(block_id)
    } else {
        None
    };
    let json_output = if let Some(pos) = args.iter().position(|arg| arg == "--json") {
        args.remove(pos);
        true
    } else {
        false
    };
    let show_graph = if let Some(pos) = args.iter().position(|arg| arg == "--graph") {
        args.remove(pos);
        true
    } else {
        false
    };

    let db_path = args.get(1).unwrap_or_else(|| print_help_and_exit());
    let txid_hex = args.get(2).unwrap_or_else(|| print_help_and_exit());
    let txid = parse_txid_hex(txid_hex).unwrap_or_else(|e| {
        eprintln!("{e}");
        process::exit(1);
    });

    let (tx, index_block_hash) = load_tx_from_chainstate(db_path, &txid, block_id.as_ref(), conf)
        .unwrap_or_else(|e| {
            eprintln!("{e}");
            process::exit(1);
        });
    let report =
        analyze_tx_effects_with_chainstate(db_path, &tx, conf, show_graph).unwrap_or_else(|e| {
            eprintln!("{e}");
            process::exit(1);
        });
    print_txid_effects_report(&report, &index_block_hash, json_output, show_graph);
}

#[derive(Debug)]
struct CallGraphEdge {
    from_contract: QualifiedContractIdentifier,
    from_function: ClarityName,
    to_contract: ContractReference,
    to_function: ClarityName,
    certainty: &'static str,
}

#[derive(Debug)]
struct TxEffectsReport {
    txid: Txid,
    label: String,
    effects: Option<FunctionEffects>,
    unresolved_contract_calls: usize,
    note: Option<String>,
    call_graph: Vec<CallGraphEdge>,
    call_contract: Option<String>,
    call_function: Option<String>,
    call_args: Option<Vec<String>>,
}

fn analyze_tx_effects_with_chainstate(
    db_path: &str,
    tx: &StacksTransaction,
    conf: Option<&Config>,
    include_graph: bool,
) -> Result<TxEffectsReport, String> {
    let conf = conf.unwrap_or(&DEFAULT_MAINNET_CONFIG);
    let data_root = db_path.to_string();
    let chain_state_path = format!("{data_root}/chainstate/");
    let sort_db_path = format!("{data_root}/burnchain/sortition");
    if !Path::new(&chain_state_path).exists() || !Path::new(&sort_db_path).exists() {
        return Err(format!(
            "Chainstate not found at {chain_state_path} (or sortition at {sort_db_path})."
        ));
    }

    let burnchain = conf.get_burnchain();
    let mut boot_data = ChainStateBootData::new(&burnchain, vec![], None);
    let (mut chainstate, _) = StacksChainState::open_and_exec(
        conf.is_mainnet(),
        conf.burnchain.chain_id,
        &chain_state_path,
        Some(&mut boot_data),
        None,
    )
    .map_err(|e| format!("Failed to open chainstate at {chain_state_path}: {e:?}"))?;
    let sort_db = SortitionDB::open(&sort_db_path, false, burnchain.pox_constants.clone())
        .map_err(|e| format!("Failed to open sortition DB at {sort_db_path}: {e:?}"))?;

    let chain_tip = SortitionDB::get_canonical_burn_chain_tip(sort_db.conn())
        .map_err(|e| format!("Failed to get sortition chain tip: {e}"))?;
    let burn_dbconn = sort_db.index_handle(&chain_tip.sortition_id);
    let parent_header = NakamotoChainState::get_canonical_block_header(chainstate.db(), &sort_db)
        .map_err(|e| format!("Failed to get chain tip: {e}"))?
        .ok_or_else(|| "No chain tip found".to_string())?;
    let stacks_tip = parent_header.index_block_hash();

    let analysis_result = chainstate
        .maybe_read_only_clarity_tx(&burn_dbconn, &stacks_tip, |clarity_tx| {
            analyze_tx_effects(clarity_tx, tx, include_graph)
        })
        .map_err(|e| format!("Failed to read chainstate: {e:?}"))?;

    analysis_result.ok_or_else(|| "Chain tip not found in chainstate.".to_string())
}

fn analyze_tx_effects(
    clarity_tx: &mut ClarityReadOnlyConnection,
    tx: &StacksTransaction,
    include_graph: bool,
) -> TxEffectsReport {
    let txid = tx.txid();
    let label = tx_label(tx);
    let mut contract_effects: BTreeMap<
        QualifiedContractIdentifier,
        BTreeMap<ClarityName, FunctionEffects>,
    > = BTreeMap::new();

    match &tx.payload {
        TransactionPayload::ContractCall(call) => {
            let contract_id = call.contract_identifier();
            let call_contract = Some(contract_id.to_string());
            let call_function = Some(call.function_name.to_string());
            let call_args = Some(
                call.function_args
                    .iter()
                    .map(|arg| arg.to_string())
                    .collect::<Vec<_>>(),
            );
            let effects_map =
                match load_contract_effects(clarity_tx, &contract_id, &mut contract_effects) {
                    Ok(Some(())) => contract_effects.get(&contract_id),
                    Ok(None) => None,
                    Err(err) => {
                        return TxEffectsReport {
                            txid,
                            label,
                            effects: None,
                            unresolved_contract_calls: 0,
                            note: Some(err),
                            call_graph: Vec::new(),
                            call_contract,
                            call_function,
                            call_args,
                        };
                    }
                };
            let Some(effects_map) = effects_map else {
                return TxEffectsReport {
                    txid,
                    label,
                    effects: None,
                    unresolved_contract_calls: 0,
                    note: Some("missing contract source".to_string()),
                    call_graph: Vec::new(),
                    call_contract,
                    call_function,
                    call_args,
                };
            };
            let Some(root_effects) = effects_map.get(&call.function_name).cloned() else {
                return TxEffectsReport {
                    txid,
                    label,
                    effects: None,
                    unresolved_contract_calls: 0,
                    note: Some("missing function analysis".to_string()),
                    call_graph: Vec::new(),
                    call_contract,
                    call_function,
                    call_args,
                };
            };

            let arg_contracts: Vec<Option<QualifiedContractIdentifier>> = call
                .function_args
                .iter()
                .map(extract_contract_arg)
                .collect();
            let arg_principals: Vec<Option<PrincipalData>> = call
                .function_args
                .iter()
                .map(extract_principal_arg)
                .collect();

            let mut contracts =
                collect_contract_effects_recursive(clarity_tx, &contract_id, effects_map)
                    .unwrap_or_default();
            let call_graph = if include_graph {
                build_contract_call_graph(&contract_id, &call.function_name, &contracts)
                    .into_iter()
                    .map(|mut edge| {
                        edge.to_contract =
                            resolve_contract_reference(&edge.to_contract, &arg_contracts);
                        edge
                    })
                    .collect()
            } else {
                Vec::new()
            };

            let mut resolved = match resolve_contract_calls_across_contracts(
                &root_effects,
                &arg_contracts,
                &mut contracts,
                clarity_tx,
            ) {
                Ok(value) => value,
                Err(err) => {
                    return TxEffectsReport {
                        txid,
                        label,
                        effects: None,
                        unresolved_contract_calls: 0,
                        note: Some(err),
                        call_graph,
                        call_contract,
                        call_function,
                        call_args,
                    };
                }
            };
            resolved = resolve_principal_effects(&resolved, &arg_principals);
            apply_tx_prereqs(&mut resolved, tx);

            let unresolved = resolved.contract_calls.len();
            TxEffectsReport {
                txid,
                label,
                effects: Some(resolved),
                unresolved_contract_calls: unresolved,
                note: None,
                call_graph,
                call_contract,
                call_function,
                call_args,
            }
        }
        TransactionPayload::SmartContract(contract, clarity_version_opt) => {
            let Some(contract_id) = contract_deploy_identifier(tx, contract) else {
                return TxEffectsReport {
                    txid,
                    label,
                    effects: None,
                    unresolved_contract_calls: 0,
                    note: Some("missing origin principal".to_string()),
                    call_graph: Vec::new(),
                    call_contract: None,
                    call_function: None,
                    call_args: None,
                };
            };
            let epoch = clarity_tx.get_epoch();
            let clarity_version =
                (*clarity_version_opt).unwrap_or_else(|| ClarityVersion::default_for_epoch(epoch));
            let contract_source = contract.code_body.to_string();
            let mut cost_track = LimitedCostTracker::new_free();
            let contract_ast = match build_ast(
                &contract_id,
                &contract_source,
                &mut cost_track,
                clarity_version,
                epoch,
            ) {
                Ok(value) => value,
                Err(err) => {
                    return TxEffectsReport {
                        txid,
                        label,
                        effects: None,
                        unresolved_contract_calls: 0,
                        note: Some(format!("Failed to parse contract: {err}")),
                        call_graph: Vec::new(),
                        call_contract: None,
                        call_function: None,
                        call_args: None,
                    };
                }
            };
            let analysis_result = clarity_tx.with_analysis_db_readonly(|db| {
                run_analysis(
                    &contract_id,
                    &contract_ast.expressions,
                    db,
                    false,
                    cost_track,
                    epoch,
                    clarity_version,
                    false,
                )
            });
            let analysis = match analysis_result {
                Ok(value) => value,
                Err(err) => {
                    return TxEffectsReport {
                        txid,
                        label,
                        effects: None,
                        unresolved_contract_calls: 0,
                        note: Some(format!("Failed to analyze contract: {}", err.0)),
                        call_graph: Vec::new(),
                        call_contract: None,
                        call_function: None,
                        call_args: None,
                    };
                }
            };
            let deploy_effects = match EffectsAnalyzer::compute_deploy_effects(&analysis) {
                Ok(value) => value,
                Err(err) => {
                    return TxEffectsReport {
                        txid,
                        label,
                        effects: None,
                        unresolved_contract_calls: 0,
                        note: Some(format!("Failed to analyze deploy effects: {err}")),
                        call_graph: Vec::new(),
                        call_contract: None,
                        call_function: None,
                        call_args: None,
                    };
                }
            };

            let deploy_name = ClarityName::from("deploy");
            let mut root_effects = BTreeMap::new();
            root_effects.insert(deploy_name.clone(), deploy_effects.clone());
            let mut contracts =
                collect_contract_effects_recursive(clarity_tx, &contract_id, &root_effects)
                    .unwrap_or_default();
            let call_graph = if include_graph {
                build_contract_call_graph(&contract_id, &deploy_name, &contracts)
            } else {
                Vec::new()
            };

            let mut resolved = match resolve_contract_calls_across_contracts(
                &deploy_effects,
                &[],
                &mut contracts,
                clarity_tx,
            ) {
                Ok(value) => value,
                Err(err) => {
                    return TxEffectsReport {
                        txid,
                        label,
                        effects: None,
                        unresolved_contract_calls: 0,
                        note: Some(err),
                        call_graph,
                        call_contract: None,
                        call_function: None,
                        call_args: None,
                    };
                }
            };
            apply_tx_prereqs(&mut resolved, tx);
            let unresolved = resolved.contract_calls.len();
            TxEffectsReport {
                txid,
                label,
                effects: Some(resolved),
                unresolved_contract_calls: unresolved,
                note: None,
                call_graph,
                call_contract: None,
                call_function: None,
                call_args: None,
            }
        }
        TransactionPayload::TokenTransfer(recipient, ..) => {
            let mut effects = stx_transfer_effects(tx, recipient);
            apply_tx_prereqs(&mut effects, tx);
            TxEffectsReport {
                txid,
                label,
                effects: Some(effects),
                unresolved_contract_calls: 0,
                note: None,
                call_graph: Vec::new(),
                call_contract: None,
                call_function: None,
                call_args: None,
            }
        }
        TransactionPayload::Coinbase(_payload, recipient_opt, _vrf_opt) => {
            let mut effects = coinbase_effects(tx, recipient_opt.as_ref());
            apply_tx_prereqs(&mut effects, tx);
            TxEffectsReport {
                txid,
                label,
                effects: Some(effects),
                unresolved_contract_calls: 0,
                note: None,
                call_graph: Vec::new(),
                call_contract: None,
                call_function: None,
                call_args: None,
            }
        }
        TransactionPayload::TenureChange(_payload) => {
            let mut effects = tenure_change_effects();
            apply_tx_prereqs(&mut effects, tx);
            TxEffectsReport {
                txid,
                label,
                effects: Some(effects),
                unresolved_contract_calls: 0,
                note: None,
                call_graph: Vec::new(),
                call_contract: None,
                call_function: None,
                call_args: None,
            }
        }
        _ => TxEffectsReport {
            txid,
            label,
            effects: None,
            unresolved_contract_calls: 0,
            note: Some("unsupported payload".to_string()),
            call_graph: Vec::new(),
            call_contract: None,
            call_function: None,
            call_args: None,
        },
    }
}

fn build_contract_call_graph(
    root_contract: &QualifiedContractIdentifier,
    root_function: &ClarityName,
    contracts: &BTreeMap<QualifiedContractIdentifier, BTreeMap<ClarityName, FunctionEffects>>,
) -> Vec<CallGraphEdge> {
    let mut edges = Vec::new();
    let mut visited = BTreeSet::new();
    let mut queue = vec![(root_contract.clone(), root_function.clone())];

    while let Some((contract_id, function_name)) = queue.pop() {
        if !visited.insert((contract_id.clone(), function_name.clone())) {
            continue;
        }
        let Some(functions) = contracts.get(&contract_id) else {
            continue;
        };
        let Some(effects) = functions.get(&function_name) else {
            continue;
        };
        for call in effects.contract_calls.iter() {
            edges.push(CallGraphEdge {
                from_contract: contract_id.clone(),
                from_function: function_name.clone(),
                to_contract: call.contract.clone(),
                to_function: call.function.clone(),
                certainty: "might",
            });
            if let ContractReference::Literal(callee_contract) = &call.contract {
                queue.push((callee_contract.clone(), call.function.clone()));
            }
        }
    }

    edges
}

fn print_tx_effects_report(report: &TxEffectsReport, json_output: bool, show_graph: bool) {
    if json_output {
        let call_graph = report
            .call_graph
            .iter()
            .map(|edge| {
                json!({
                    "from_contract": edge.from_contract.to_string(),
                    "from_function": edge.from_function.to_string(),
                    "to_contract": format_contract_reference(&edge.to_contract),
                    "to_function": edge.to_function.to_string(),
                    "certainty": edge.certainty,
                })
            })
            .collect::<Vec<_>>();
        let effects = report
            .effects
            .as_ref()
            .map(function_effects_to_json)
            .unwrap_or(JsonValue::Null);
        let payload = json!({
            "txid": report.txid.to_hex(),
            "label": report.label,
            "effects": effects,
            "unresolved_contract_calls": report.unresolved_contract_calls,
            "note": report.note,
            "call_graph": call_graph,
            "call_contract": report.call_contract,
            "call_function": report.call_function,
            "call_args": report.call_args,
        });
        println!("{}", serde_json::to_string_pretty(&payload).unwrap());
        return;
    }

    println!("Txid: {}", report.txid.to_hex());
    println!("Type: {}", report.label);
    if let Some(note) = &report.note {
        println!("Note: {note}");
    }
    if let Some(effects) = &report.effects {
        print_effects_section("reads", &effects.reads);
        print_effects_section("writes", &effects.writes);
        if report.unresolved_contract_calls > 0 {
            println!(
                "  unresolved contract calls: {}",
                report.unresolved_contract_calls
            );
        }
    }
    if show_graph && !report.call_graph.is_empty() {
        println!("  call-graph (might-call):");
        for edge in report.call_graph.iter() {
            println!(
                "    - {}.{} -> {}.{}",
                edge.from_contract,
                edge.from_function,
                format_contract_reference(&edge.to_contract),
                edge.to_function
            );
        }
    }
}

fn print_txid_effects_report(
    report: &TxEffectsReport,
    index_block_hash: &str,
    json_output: bool,
    show_graph: bool,
) {
    if json_output {
        let call_graph = report
            .call_graph
            .iter()
            .map(|edge| {
                json!({
                    "from_contract": edge.from_contract.to_string(),
                    "from_function": edge.from_function.to_string(),
                    "to_contract": format_contract_reference(&edge.to_contract),
                    "to_function": edge.to_function.to_string(),
                    "certainty": edge.certainty,
                })
            })
            .collect::<Vec<_>>();
        let effects = report
            .effects
            .as_ref()
            .map(function_effects_to_json)
            .unwrap_or(JsonValue::Null);
        let payload = json!({
            "txid": report.txid.to_hex(),
            "index_block_hash": index_block_hash,
            "label": report.label,
            "effects": effects,
            "unresolved_contract_calls": report.unresolved_contract_calls,
            "note": report.note,
            "call_graph": call_graph,
            "call_contract": report.call_contract,
            "call_function": report.call_function,
            "call_args": report.call_args,
        });
        println!("{}", serde_json::to_string_pretty(&payload).unwrap());
        return;
    }

    println!(
        "Txid: {} (block {})",
        report.txid.to_hex(),
        index_block_hash
    );
    println!("Type: {}", report.label);
    if let Some(note) = &report.note {
        println!("Note: {note}");
    }
    if let Some(effects) = &report.effects {
        print_effects_section("reads", &effects.reads);
        print_effects_section("writes", &effects.writes);
        if report.unresolved_contract_calls > 0 {
            println!(
                "  unresolved contract calls: {}",
                report.unresolved_contract_calls
            );
        }
    }
    if show_graph && !report.call_graph.is_empty() {
        println!("  call-graph (might-call):");
        for edge in report.call_graph.iter() {
            println!(
                "    - {}.{} -> {}.{}",
                edge.from_contract,
                edge.from_function,
                format_contract_reference(&edge.to_contract),
                edge.to_function
            );
        }
    }
}

// Label transactions with a human-readable summary.
fn tx_label(tx: &StacksTransaction) -> String {
    match &tx.payload {
        TransactionPayload::ContractCall(call) => {
            format!(
                "contract-call {}.{}",
                call.contract_identifier(),
                call.function_name
            )
        }
        TransactionPayload::SmartContract(contract, _) => {
            format!("deploy {}", contract.name)
        }
        TransactionPayload::TokenTransfer(recipient, ..) => {
            format!("stx-transfer {}", recipient)
        }
        _ => tx.payload.name().to_string(),
    }
}

// Build effects for an STX transfer using sender and recipient principals.
fn stx_transfer_effects(tx: &StacksTransaction, recipient: &PrincipalData) -> FunctionEffects {
    let mut effects = FunctionEffects {
        purity: Purity::Impure,
        ..Default::default()
    };
    let sender = origin_principal(tx)
        .map(PrincipalReference::Literal)
        .unwrap_or(PrincipalReference::Any);
    let recipient = PrincipalReference::Literal(recipient.clone());
    for principal in [sender, recipient] {
        let access = EffectTarget::AssetOwnership(AssetOwnershipAccess {
            asset: AssetId::Stx,
            principal,
        });
        effects.reads.insert(access.clone());
        effects.writes.insert(access);
    }
    effects
}

// Add nonce and fee-payer balance effects for transaction validity.
fn apply_tx_prereqs(effects: &mut FunctionEffects, tx: &StacksTransaction) {
    let origin = origin_principal(tx)
        .map(PrincipalReference::Literal)
        .unwrap_or(PrincipalReference::Any);
    let nonce_access = EffectTarget::AccountNonce(AccountNonceAccess { principal: origin });
    effects.reads.insert(nonce_access.clone());
    effects.writes.insert(nonce_access);

    if let Some(sponsor) = sponsor_principal(tx) {
        let principal = PrincipalReference::Literal(sponsor);
        let sponsor_nonce = EffectTarget::AccountNonce(AccountNonceAccess { principal });
        effects.reads.insert(sponsor_nonce.clone());
        effects.writes.insert(sponsor_nonce);
    }

    let payer = payer_principal(tx)
        .map(PrincipalReference::Literal)
        .unwrap_or(PrincipalReference::Any);
    let access = EffectTarget::AssetOwnership(AssetOwnershipAccess {
        asset: AssetId::stx(),
        principal: payer,
    });
    effects.reads.insert(access.clone());
    effects.writes.insert(access);
}

// Build effects for a coinbase transaction.
fn coinbase_effects(tx: &StacksTransaction, recipient: Option<&PrincipalData>) -> FunctionEffects {
    let mut effects = FunctionEffects {
        purity: Purity::Impure,
        ..Default::default()
    };
    let principal = recipient
        .cloned()
        .or_else(|| origin_principal(tx))
        .map(PrincipalReference::Literal)
        .unwrap_or(PrincipalReference::Any);
    let access = EffectTarget::AssetOwnership(AssetOwnershipAccess {
        asset: AssetId::Stx,
        principal,
    });
    effects.reads.insert(access.clone());
    effects.writes.insert(access);
    effects
}

// Build effects for a tenure-change transaction.
fn tenure_change_effects() -> FunctionEffects {
    let mut effects = FunctionEffects {
        purity: Purity::Impure,
        ..Default::default()
    };
    effects
        .writes
        .insert(EffectTarget::ChainState(ChainStateRead::TenureInfo));
    effects
}

// Extract a contract identifier from a Clarity value, if present.
fn extract_contract_arg(value: &Value) -> Option<QualifiedContractIdentifier> {
    match value {
        Value::Principal(PrincipalData::Contract(contract_id)) => Some(contract_id.clone()),
        Value::CallableContract(callable) => Some(callable.contract_identifier.clone()),
        _ => None,
    }
}

// Extract a principal from a Clarity value, if present.
fn extract_principal_arg(value: &Value) -> Option<PrincipalData> {
    match value {
        Value::Principal(principal) => Some(principal.clone()),
        _ => None,
    }
}

// Resolve principal arguments within effect targets.
fn resolve_principal_effects(
    effects: &FunctionEffects,
    arg_principals: &[Option<PrincipalData>],
) -> FunctionEffects {
    let mut resolved = effects.clone();
    resolved.reads = resolve_principal_effect_set(&effects.reads, arg_principals);
    resolved.writes = resolve_principal_effect_set(&effects.writes, arg_principals);
    resolved
}

// Resolve principal arguments for an entire effect set.
fn resolve_principal_effect_set(
    effects: &BTreeSet<EffectTarget>,
    arg_principals: &[Option<PrincipalData>],
) -> BTreeSet<EffectTarget> {
    effects
        .iter()
        .map(|effect| match effect {
            EffectTarget::AssetOwnership(access) => {
                let principal = resolve_principal_reference(&access.principal, arg_principals);
                EffectTarget::AssetOwnership(AssetOwnershipAccess {
                    asset: access.asset.clone(),
                    principal,
                })
            }
            EffectTarget::AccountNonce(access) => {
                let principal = resolve_principal_reference(&access.principal, arg_principals);
                EffectTarget::AccountNonce(AccountNonceAccess { principal })
            }
            other => other.clone(),
        })
        .collect()
}

// Resolve a principal reference using transaction arguments.
fn resolve_principal_reference(
    reference: &PrincipalReference,
    arg_principals: &[Option<PrincipalData>],
) -> PrincipalReference {
    match reference {
        PrincipalReference::Argument(index) => arg_principals
            .get(*index as usize)
            .and_then(|value| value.clone())
            .map(PrincipalReference::Literal)
            .unwrap_or(PrincipalReference::Any),
        other => other.clone(),
    }
}

// Resolve a contract reference using transaction arguments.
fn resolve_contract_reference(
    reference: &ContractReference,
    arg_contracts: &[Option<QualifiedContractIdentifier>],
) -> ContractReference {
    match reference {
        ContractReference::Argument(index) => arg_contracts
            .get(*index as usize)
            .and_then(|value| value.clone())
            .map(ContractReference::Literal)
            .unwrap_or(ContractReference::Any),
        _ => reference.clone(),
    }
}

// Cache contract analysis results and indicate whether effects are available.
fn load_contract_effects(
    clarity_tx: &mut ClarityReadOnlyConnection,
    contract_id: &QualifiedContractIdentifier,
    contract_effects: &mut BTreeMap<
        QualifiedContractIdentifier,
        BTreeMap<ClarityName, FunctionEffects>,
    >,
) -> Result<Option<()>, String> {
    if !contract_effects.contains_key(contract_id) {
        let contract_source =
            clarity_tx.with_clarity_db_readonly(|db| db.get_contract_src(contract_id));
        let Some(contract_source) = contract_source else {
            return Ok(None);
        };

        let clarity_version = clarity_tx
            .with_analysis_db_readonly(|db| db.get_clarity_version(contract_id).ok())
            .unwrap_or_else(|| ClarityVersion::default_for_epoch(clarity_tx.get_epoch()));
        let epoch = clarity_tx.get_epoch();
        let mut cost_track = LimitedCostTracker::new_free();
        let contract_ast = build_ast(
            contract_id,
            &contract_source,
            &mut cost_track,
            clarity_version,
            epoch,
        )
        .map_err(|e| format!("Failed to parse contract {contract_id}: {e}"))?;
        let analysis_result = clarity_tx.with_analysis_db_readonly(|db| {
            run_analysis(
                contract_id,
                &contract_ast.expressions,
                db,
                false,
                cost_track,
                epoch,
                clarity_version,
                false,
            )
        });
        let analysis = analysis_result
            .map_err(|e| format!("Failed to analyze contract {contract_id}: {}", e.0))?;
        contract_effects.insert(contract_id.clone(), analysis.function_effects);
    }
    Ok(Some(()))
}

// Resolve contract-call effects by loading referenced contracts until no further progress.
fn resolve_contract_calls_across_contracts(
    effects: &FunctionEffects,
    arg_contracts: &[Option<QualifiedContractIdentifier>],
    contract_effects: &mut BTreeMap<
        QualifiedContractIdentifier,
        BTreeMap<ClarityName, FunctionEffects>,
    >,
    clarity_tx: &mut ClarityReadOnlyConnection,
) -> Result<FunctionEffects, String> {
    resolve_contract_effects_transitively(contract_effects);
    let mut resolved = effects.resolve_contract_calls(arg_contracts, contract_effects);
    loop {
        let mut loaded_any = false;
        let calls: Vec<ContractCall> = resolved.contract_calls.iter().cloned().collect();
        for call in calls {
            if let ContractReference::Literal(contract_id) = call.contract
                && !contract_effects.contains_key(&contract_id)
            {
                if (load_contract_effects(clarity_tx, &contract_id, contract_effects)?).is_none() {
                    continue;
                }
                loaded_any = true;
            }
        }
        if !loaded_any {
            break;
        }
        resolve_contract_effects_transitively(contract_effects);
        let next = resolved.resolve_contract_calls(arg_contracts, contract_effects);
        if next == resolved {
            break;
        }
        resolved = next;
    }
    Ok(resolved)
}

// Build a principal from the transaction origin if possible.
fn origin_principal(tx: &StacksTransaction) -> Option<PrincipalData> {
    let origin = tx.get_origin();
    let addr = if tx.is_mainnet() {
        origin.address_mainnet()
    } else {
        origin.address_testnet()
    };
    Some(PrincipalData::from(addr))
}

// Build the contract identifier for a smart contract deploy.
fn contract_deploy_identifier(
    tx: &StacksTransaction,
    contract: &TransactionSmartContract,
) -> Option<QualifiedContractIdentifier> {
    let origin = origin_principal(tx)?;
    let issuer = match origin {
        PrincipalData::Standard(issuer) => issuer,
        _ => return None,
    };
    Some(QualifiedContractIdentifier::new(
        issuer,
        contract.name.clone(),
    ))
}

// Build a principal from the fee payer account.
fn payer_principal(tx: &StacksTransaction) -> Option<PrincipalData> {
    let payer = tx.get_payer();
    Some(spending_condition_principal(&payer, tx.is_mainnet()))
}

// Build a principal for the sponsor account, if any.
fn sponsor_principal(tx: &StacksTransaction) -> Option<PrincipalData> {
    tx.auth
        .sponsor()
        .map(|condition| spending_condition_principal(condition, tx.is_mainnet()))
}

// Convert a spending condition into a principal based on network.
fn spending_condition_principal(
    condition: &TransactionSpendingCondition,
    is_mainnet: bool,
) -> PrincipalData {
    let addr = if is_mainnet {
        condition.address_mainnet()
    } else {
        condition.address_testnet()
    };
    PrincipalData::from(addr)
}

fn read_hex_arg(arg: &str) -> Result<String, String> {
    let trimmed = arg.trim();
    if trimmed == "-" {
        let mut input = String::new();
        io::stdin()
            .read_to_string(&mut input)
            .map_err(|e| format!("Failed to read stdin: {e}"))?;
        return Ok(input);
    }
    let path = if let Some(stripped) = trimmed.strip_prefix('@') {
        stripped
    } else {
        trimmed
    };
    if Path::new(path).exists() {
        fs::read_to_string(path).map_err(|e| format!("Failed to read {path}: {e}"))
    } else {
        Ok(trimmed.to_string())
    }
}

fn parse_tx_from_hex(tx_hex: &str) -> Result<StacksTransaction, String> {
    let bytes = hex_bytes(tx_hex.trim()).map_err(|e| format!("Invalid hex: {e:?}"))?;
    let mut cursor = &bytes[..];
    StacksTransaction::consensus_deserialize(&mut cursor)
        .map_err(|e| format!("Failed to parse tx: {e:?}"))
}

fn parse_txid_hex(txid_hex: &str) -> Result<Txid, String> {
    let bytes = hex_bytes(txid_hex.trim()).map_err(|e| format!("Invalid txid: {e:?}"))?;
    if bytes.len() != 32 {
        return Err(format!(
            "Invalid txid length: expected 32 bytes, got {}",
            bytes.len()
        ));
    }
    let mut buf = [0u8; 32];
    buf.copy_from_slice(&bytes);
    Ok(Txid(buf))
}

fn parse_block_id_hex(block_hex: &str) -> Result<StacksBlockId, String> {
    StacksBlockId::from_hex(block_hex.trim()).map_err(|e| format!("Invalid block id: {e:?}"))
}

fn load_tx_from_chainstate(
    db_path: &str,
    txid: &Txid,
    block_id: Option<&StacksBlockId>,
    conf: Option<&Config>,
) -> Result<(StacksTransaction, String), String> {
    let conf = conf.unwrap_or(&DEFAULT_MAINNET_CONFIG);
    let chain_state_path = format!("{db_path}/chainstate/");
    let (chainstate, _) = StacksChainState::open(
        conf.is_mainnet(),
        conf.burnchain.chain_id,
        &chain_state_path,
        None,
    )
    .map_err(|e| format!("Failed to open chainstate at {chain_state_path}: {e:?}"))?;

    if let Some(block_id) = block_id {
        return load_tx_from_block(&chainstate, txid, block_id);
    }

    let conn = chainstate.db();
    let sql = "SELECT tx_hex, index_block_hash FROM transactions WHERE txid = ?1 LIMIT 1";
    let txid_hex = txid.to_hex();
    let row: Option<(String, String)> = conn
        .query_row(sql, params![txid_hex], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()
        .map_err(|e| format!("Failed to query transactions: {e}"))?;
    let Some((tx_hex, index_block_hash)) = row else {
        return Err(format!(
            "Transaction {txid_hex} not found in chainstate (txindex may be disabled). \
Provide --block-id to scan a specific block."
        ));
    };
    let tx = parse_tx_from_hex(&tx_hex)?;
    Ok((tx, index_block_hash))
}

fn load_tx_from_block(
    chainstate: &StacksChainState,
    txid: &Txid,
    block_id: &StacksBlockId,
) -> Result<(StacksTransaction, String), String> {
    if let Some((block, _)) = chainstate
        .nakamoto_blocks_db()
        .get_nakamoto_block(block_id)
        .map_err(|e| format!("Failed to load Nakamoto block {block_id}: {e:?}"))?
    {
        if let Some(tx) = block.txs.into_iter().find(|tx| &tx.txid() == txid) {
            return Ok((tx, block_id.to_string()));
        }
        return Err(format!(
            "Transaction {} not found in Nakamoto block {block_id}.",
            txid.to_hex()
        ));
    }

    let has_block = StacksChainState::has_block_indexed(&chainstate.blocks_path, block_id)
        .map_err(|e| format!("Failed to check chunk store for {block_id}: {e:?}"))?;
    if !has_block {
        return Err(format!(
            "Block {block_id} not found in chainstate (no Nakamoto block and no chunk store file)."
        ));
    }

    let block_path = StacksChainState::get_index_block_path(&chainstate.blocks_path, block_id)
        .map_err(|e| format!("Failed to resolve block path for {block_id}: {e:?}"))?;
    let block: StacksBlock = StacksChainState::consensus_load(&block_path)
        .map_err(|e| format!("Failed to load block {block_id} from chunk store: {e:?}"))?;
    if let Some(tx) = block.txs.into_iter().find(|tx| &tx.txid() == txid) {
        return Ok((tx, block_id.to_string()));
    }
    Err(format!(
        "Transaction {} not found in block {block_id}.",
        txid.to_hex()
    ))
}

fn print_effects_section(label: &str, effects: &std::collections::BTreeSet<EffectTarget>) {
    if effects.is_empty() {
        return;
    }
    println!("  {label}:");
    for effect in effects {
        println!("    - {}", format_effect_target(effect));
    }
}

// Collect contract analyses starting from a root contract and following literal contract calls.
fn collect_contract_effects_recursive(
    clarity_tx: &mut ClarityReadOnlyConnection,
    root_contract: &QualifiedContractIdentifier,
    root_effects: &BTreeMap<ClarityName, FunctionEffects>,
) -> Result<BTreeMap<QualifiedContractIdentifier, BTreeMap<ClarityName, FunctionEffects>>, String> {
    let mut contracts = BTreeMap::new();
    contracts.insert(root_contract.clone(), root_effects.clone());
    let mut queue = vec![root_contract.clone()];
    let mut visited = BTreeSet::new();

    while let Some(contract_id) = queue.pop() {
        if !visited.insert(contract_id.clone()) {
            continue;
        }
        let callees = contracts
            .get(&contract_id)
            .map(|functions| {
                functions
                    .values()
                    .flat_map(|effects| effects.contract_calls.iter())
                    .filter_map(|call| match &call.contract {
                        ContractReference::Literal(id) => Some(id.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        for callee in callees {
            if contracts.contains_key(&callee) {
                continue;
            }
            if load_contract_effects(clarity_tx, &callee, &mut contracts)?.is_some() {
                queue.push(callee);
            }
        }
    }

    Ok(contracts)
}

// Resolve contract-call effects transitively across a set of contracts.
fn resolve_contract_effects_transitively(
    contracts: &mut BTreeMap<QualifiedContractIdentifier, BTreeMap<ClarityName, FunctionEffects>>,
) {
    let mut changed = true;
    while changed {
        changed = false;
        let snapshot = contracts.clone();
        for functions in contracts.values_mut() {
            for effects in functions.values_mut() {
                let resolved = effects.resolve_contract_calls(&[], &snapshot);
                if *effects != resolved {
                    *effects = resolved;
                    changed = true;
                }
            }
        }
    }
}

fn format_effect_target(effect: &EffectTarget) -> String {
    match effect {
        EffectTarget::Contract(access) => {
            let contract = format_contract_reference(&access.contract);
            let location = match &access.location {
                Some(StorageLocation::DataMap(name)) => format!("map {name}"),
                Some(StorageLocation::DataVar(name)) => format!("var {name}"),
                None => "any".to_string(),
            };
            format!("contract {contract} ({location})")
        }
        EffectTarget::AssetOwnership(access) => {
            let asset = format_asset_id(&access.asset);
            let principal = format_principal_reference(&access.principal);
            format!("asset {asset} principal {principal}")
        }
        EffectTarget::AccountNonce(access) => {
            let principal = format_principal_reference(&access.principal);
            format!("account-nonce {principal}")
        }
        EffectTarget::ChainState(read) => format!("chain-state {}", format_chain_read(read)),
    }
}

fn effect_target_to_json(effect: &EffectTarget) -> JsonValue {
    match effect {
        EffectTarget::Contract(access) => {
            let contract = format_contract_reference(&access.contract);
            let location = match &access.location {
                Some(StorageLocation::DataMap(name)) => json!({"DataMap": name.to_string()}),
                Some(StorageLocation::DataVar(name)) => json!({"DataVar": name.to_string()}),
                None => JsonValue::Null,
            };
            json!({
                "Contract": {
                    "contract": contract,
                    "location": location,
                }
            })
        }
        EffectTarget::AssetOwnership(access) => {
            let asset = format_asset_id(&access.asset);
            let principal = format_principal_reference(&access.principal);
            json!({
                "AssetOwnership": {
                    "asset": asset,
                    "principal": principal,
                }
            })
        }
        EffectTarget::AccountNonce(access) => {
            let principal = format_principal_reference(&access.principal);
            json!({
                "AccountNonce": {
                    "principal": principal,
                }
            })
        }
        EffectTarget::ChainState(read) => json!({
            "ChainState": format_chain_read(read),
        }),
    }
}

fn effect_set_to_json(effects: &BTreeSet<EffectTarget>) -> JsonValue {
    let entries = effects
        .iter()
        .map(effect_target_to_json)
        .collect::<Vec<_>>();
    JsonValue::Array(entries)
}

fn function_effects_to_json(effects: &FunctionEffects) -> JsonValue {
    let calls = effects
        .contract_calls
        .iter()
        .map(|call| {
            json!({
                "contract": format_contract_reference(&call.contract),
                "function": call.function.to_string(),
            })
        })
        .collect::<Vec<_>>();
    json!({
        "purity": format!("{:?}", effects.purity),
        "reads": effect_set_to_json(&effects.reads),
        "writes": effect_set_to_json(&effects.writes),
        "contract_calls": calls,
    })
}

fn format_contract_reference(reference: &ContractReference) -> String {
    match reference {
        ContractReference::Any => "<any>".to_string(),
        ContractReference::Literal(contract) => contract.to_string(),
        ContractReference::Argument(index) => format!("$arg[{index}]"),
    }
}

fn format_principal_reference(reference: &PrincipalReference) -> String {
    match reference {
        PrincipalReference::Any => "<any>".to_string(),
        PrincipalReference::Literal(principal) => principal.to_string(),
        PrincipalReference::Argument(index) => format!("$arg[{index}]"),
    }
}

fn format_chain_read(read: &ChainStateRead) -> &'static str {
    match read {
        ChainStateRead::BlockInfo => "block-info",
        ChainStateRead::StacksBlockInfo => "stacks-block-info",
        ChainStateRead::BurnBlockInfo => "burn-block-info",
        ChainStateRead::TenureInfo => "tenure-info",
    }
}

fn format_asset_id(asset: &AssetId) -> String {
    match asset {
        AssetId::Stx => "stx".to_string(),
        AssetId::Token {
            contract,
            name,
            kind,
        } => {
            let kind_label = match kind {
                TokenKind::Fungible => "ft",
                TokenKind::NonFungible => "nft",
            };
            format!("{contract}.{name} ({kind_label})")
        }
    }
}

/// Fetch and process a `StagingBlock` from database and call `replay_block()` to validate
fn replay_staging_block(
    db_path: &str,
    block_id: &StacksBlockId,
    conf: &Config,
) -> Result<(), String> {
    let chain_state_path = format!("{db_path}/chainstate/");
    let sort_db_path = format!("{db_path}/burnchain/sortition");

    let (mut chainstate, _) = StacksChainState::open(
        conf.is_mainnet(),
        conf.burnchain.chain_id,
        &chain_state_path,
        None,
    )
    .map_err(|e| format!("Failed to open chainstate at {chain_state_path}: {e:?}"))?;

    let burnchain = conf.get_burnchain();
    let epochs = conf.burnchain.get_epoch_list();
    let mut sortdb = SortitionDB::connect(
        &sort_db_path,
        burnchain.first_block_height,
        &burnchain.first_block_hash,
        u64::from(burnchain.first_block_timestamp),
        &epochs,
        burnchain.pox_constants.clone(),
        None,
        true,
    )
    .map_err(|e| format!("Failed to open sortition DB at {sort_db_path}: {e:?}"))?;

    let sort_tx = sortdb.tx_begin_at_tip();

    let blocks_path = chainstate.blocks_path.clone();
    let (chainstate_tx, clarity_instance) = chainstate
        .chainstate_tx_begin()
        .map_err(|e| format!("{e:?}"))?;
    let mut next_staging_block =
        StacksChainState::load_staging_block_info(&chainstate_tx.tx, block_id)
            .map_err(|e| format!("Failed to load staging block info: {e:?}"))?
            .ok_or_else(|| "No such index block hash in block database".to_string())?;

    next_staging_block.block_data = StacksChainState::load_block_bytes(
        &blocks_path,
        &next_staging_block.consensus_hash,
        &next_staging_block.anchored_block_hash,
    )
    .map_err(|e| format!("Failed to load block bytes: {e:?}"))?
    .unwrap_or_default();

    let parent_header_info =
        StacksChainState::get_parent_header_info(&chainstate_tx, &next_staging_block)
            .map_err(|e| format!("Failed to get parent header info: {e:?}"))?
            .ok_or_else(|| "Missing parent header info".to_string())?;

    let block = StacksChainState::extract_stacks_block(&next_staging_block)
        .map_err(|e| format!("{e:?}"))?;
    let block_size = next_staging_block.block_data.len() as u64;

    replay_block(
        sort_tx,
        chainstate_tx,
        clarity_instance,
        &parent_header_info,
        &next_staging_block.parent_microblock_hash,
        next_staging_block.parent_microblock_seq,
        block_id,
        &block,
        block_size,
        &next_staging_block.consensus_hash,
        &next_staging_block.anchored_block_hash,
        next_staging_block.commit_burn,
        next_staging_block.sortition_burn,
    )
}

/// Process a mock mined block and call `replay_block()` to validate
fn replay_mock_mined_block(db_path: &str, block: AssembledAnchorBlock, conf: Option<&Config>) {
    let chain_state_path = format!("{db_path}/chainstate/");
    let sort_db_path = format!("{db_path}/burnchain/sortition");

    let conf = conf.unwrap_or(&DEFAULT_MAINNET_CONFIG);

    let (mut chainstate, _) = StacksChainState::open(
        conf.is_mainnet(),
        conf.burnchain.chain_id,
        &chain_state_path,
        None,
    )
    .unwrap();

    let burnchain = conf.get_burnchain();
    let epochs = conf.burnchain.get_epoch_list();
    let mut sortdb = SortitionDB::connect(
        &sort_db_path,
        burnchain.first_block_height,
        &burnchain.first_block_hash,
        u64::from(burnchain.first_block_timestamp),
        &epochs,
        burnchain.pox_constants.clone(),
        None,
        true,
    )
    .unwrap();
    let sort_tx = sortdb.tx_begin_at_tip();

    let (chainstate_tx, clarity_instance) = chainstate
        .chainstate_tx_begin()
        .expect("Failed to start chainstate tx");

    let block_consensus_hash = &block.consensus_hash;
    let block_hash = block.anchored_block.block_hash();
    let block_id = StacksBlockId::new(block_consensus_hash, &block_hash);
    let block_size = block
        .anchored_block
        .block_size()
        .map(u64::try_from)
        .unwrap_or_else(|e| panic!("Error serializing block {block_hash}: {e}"))
        .expect("u64 overflow");

    let Some(parent_header_info) = StacksChainState::get_anchored_block_header_info(
        &chainstate_tx,
        &block.parent_consensus_hash,
        &block.anchored_block.header.parent_block,
    )
    .unwrap() else {
        println!("Failed to load parent head info for block: {block_hash}");
        return;
    };

    replay_block(
        sort_tx,
        chainstate_tx,
        clarity_instance,
        &parent_header_info,
        &block.anchored_block.header.parent_microblock,
        block.anchored_block.header.parent_microblock_sequence,
        &block_id,
        &block.anchored_block,
        block_size,
        block_consensus_hash,
        &block_hash,
        // I think the burn is used for miner rewards but not necessary for validation
        0,
        0,
    )
    .expect("Failed to replay mock mined block");
}

/// Validate a block against chainstate
#[allow(clippy::too_many_arguments)]
fn replay_block(
    mut sort_tx: IndexDBTx<SortitionHandleContext, SortitionId>,
    mut chainstate_tx: ChainstateTx,
    clarity_instance: &mut ClarityInstance,
    parent_header_info: &StacksHeaderInfo,
    parent_microblock_hash: &BlockHeaderHash,
    parent_microblock_seq: u16,
    block_id: &StacksBlockId,
    block: &StacksBlock,
    block_size: u64,
    block_consensus_hash: &ConsensusHash,
    block_hash: &BlockHeaderHash,
    block_commit_burn: u64,
    block_sortition_burn: u64,
) -> Result<(), String> {
    let parent_block_header = match &parent_header_info.anchored_header {
        StacksBlockHeaderTypes::Epoch2(bh) => bh,
        StacksBlockHeaderTypes::Nakamoto(_) => panic!("Nakamoto blocks not supported yet"),
    };
    let parent_block_hash = parent_block_header.block_hash();

    // We don't ensure that the cost is found here, because when replaying mock-mined blocks
    // there may not be a stored cost for the block.
    let cost_opt =
        StacksChainState::get_stacks_block_anchored_cost(chainstate_tx.conn(), block_id).unwrap();

    let Some(next_microblocks) = StacksChainState::inner_find_parent_microblock_stream(
        &chainstate_tx.tx,
        block_hash,
        &parent_block_hash,
        &parent_header_info.consensus_hash,
        parent_microblock_hash,
        parent_microblock_seq,
    )
    .unwrap() else {
        return Err(format!("No microblock stream found for {block_id}"));
    };

    let (burn_header_hash, burn_header_height, burn_header_timestamp, _winning_block_txid) =
        match SortitionDB::get_block_snapshot_consensus(&sort_tx, block_consensus_hash).unwrap() {
            Some(sn) => (
                sn.burn_header_hash,
                sn.block_height as u32,
                sn.burn_header_timestamp,
                sn.winning_block_txid,
            ),
            None => {
                // shouldn't happen
                panic!(
                    "CORRUPTION: staging block {block_consensus_hash}/{block_hash} does not correspond to a burn block"
                );
            }
        };

    info!(
        "Process block {}/{} = {} in burn block {}, parent microblock {}",
        block_consensus_hash, block_hash, &block_id, &burn_header_hash, parent_microblock_hash,
    );

    if !StacksChainState::check_block_attachment(parent_block_header, &block.header) {
        return Err(format!(
            "Invalid stacks block {}/{} -- does not attach to parent {}/{}",
            block_consensus_hash,
            block.block_hash(),
            parent_block_header.block_hash(),
            &parent_header_info.consensus_hash
        ));
    }

    // validation check -- validate parent microblocks and find the ones that connect the
    // block's parent to this block.
    let next_microblocks = StacksChainState::extract_connecting_microblocks(
        parent_header_info,
        block_consensus_hash,
        block_hash,
        block,
        next_microblocks,
    )
    .unwrap();
    let (last_microblock_hash, last_microblock_seq) = match next_microblocks.len() {
        0 => (EMPTY_MICROBLOCK_PARENT_HASH.clone(), 0),
        _ => {
            let l = next_microblocks.len();
            (
                next_microblocks[l - 1].block_hash(),
                next_microblocks[l - 1].header.sequence,
            )
        }
    };
    assert_eq!(*parent_microblock_hash, last_microblock_hash);
    assert_eq!(parent_microblock_seq, last_microblock_seq);

    let pox_constants = sort_tx.context.pox_constants.clone();

    match StacksChainState::append_block(
        &mut chainstate_tx,
        clarity_instance,
        &mut sort_tx,
        &pox_constants,
        parent_header_info,
        block_consensus_hash,
        &burn_header_hash,
        burn_header_height,
        burn_header_timestamp,
        block,
        block_size,
        &next_microblocks,
        block_commit_burn,
        block_sortition_burn,
        true,
    ) {
        Ok((receipt, _, _)) => {
            if let Some(cost) = cost_opt {
                if receipt.anchored_block_cost != cost {
                    return Err(format!(
                        "Failed processing block! block = {block_id}. Unexpected cost. expected = {cost}, evaluated = {}",
                        receipt.anchored_block_cost
                    ));
                }
            } else {
                info!("No stored cost for {block_id}; skipping cost check");
            }
            info!("Block processed successfully! block = {block_id}");
            Ok(())
        }
        Err(e) => Err(format!(
            "Failed processing block! block = {block_id}, error = {e:?}"
        )),
    }
}

/// Fetch and process a NakamotoBlock from database and call `replay_block_nakamoto()` to validate
fn replay_naka_staging_block(
    db_path: &str,
    block_id: &StacksBlockId,
    conf: &Config,
) -> Result<(), String> {
    let chain_state_path = format!("{db_path}/chainstate/");
    let sort_db_path = format!("{db_path}/burnchain/sortition");

    let (mut chainstate, _) = StacksChainState::open(
        conf.is_mainnet(),
        conf.burnchain.chain_id,
        &chain_state_path,
        None,
    )
    .map_err(|e| format!("Failed to open chainstate: {e:?}"))?;

    let burnchain = conf.get_burnchain();
    let epochs = conf.burnchain.get_epoch_list();
    let mut sortdb = SortitionDB::connect(
        &sort_db_path,
        burnchain.first_block_height,
        &burnchain.first_block_hash,
        u64::from(burnchain.first_block_timestamp),
        &epochs,
        burnchain.pox_constants.clone(),
        None,
        true,
    )
    .map_err(|e| format!("Failed to open sortition DB: {e:?}"))?;

    let (block, block_size) = chainstate
        .nakamoto_blocks_db()
        .get_nakamoto_block(block_id)
        .map_err(|e| format!("Failed to load Nakamoto block: {e:?}"))?
        .ok_or_else(|| "No block data found".to_string())?;

    replay_block_nakamoto(&mut sortdb, &mut chainstate, &block, block_size)
        .map_err(|e| format!("Failed to validate Nakamoto block: {e:?}"))
}

#[allow(clippy::result_large_err)]
fn replay_block_nakamoto(
    sort_db: &mut SortitionDB,
    stacks_chain_state: &mut StacksChainState,
    block: &NakamotoBlock,
    block_size: u64,
) -> Result<(), ChainstateError> {
    // find corresponding snapshot
    let next_ready_block_snapshot =
        SortitionDB::get_block_snapshot_consensus(sort_db.conn(), &block.header.consensus_hash)?
            .unwrap_or_else(|| {
                panic!(
                    "CORRUPTION: staging Nakamoto block {}/{} does not correspond to a burn block",
                    &block.header.consensus_hash,
                    &block.header.block_hash()
                )
            });

    info!("Process staging Nakamoto block";
           "consensus_hash" => %block.header.consensus_hash,
           "stacks_block_hash" => %block.header.block_hash(),
           "stacks_block_id" => %block.header.block_id(),
           "burn_block_hash" => %next_ready_block_snapshot.burn_header_hash
    );

    let Some(mut expected_total_tenure_cost) = NakamotoChainState::get_total_tenure_cost_at(
        stacks_chain_state.db(),
        &block.header.block_id(),
    )
    .unwrap() else {
        println!("Failed to find cost for block {}", block.header.block_id());
        return Ok(());
    };

    let expected_cost = match block.get_tenure_tx_payload() {
        // New block or full extend: No subtraction needed
        Some(tc) if tc.cause.is_full_extend() || tc.cause.is_new_tenure() => {
            expected_total_tenure_cost
        }

        // Partial Extend or None: We need the parent cost.
        tenure_payload => {
            let Some(mut parent_cost) = NakamotoChainState::get_total_tenure_cost_at(
                stacks_chain_state.db(),
                &block.header.parent_block_id,
            )
            .unwrap() else {
                println!(
                    "Failed to find cost for parent of block {}",
                    block.header.block_id()
                );
                return Ok(());
            };

            // If we have a partial extend, zero out that specific field in the parent cost
            if let Some(payload) = tenure_payload {
                match payload.cause {
                    TenureChangeCause::ExtendedReadCount => parent_cost.read_count = 0,
                    TenureChangeCause::ExtendedReadLength => parent_cost.read_length = 0,
                    TenureChangeCause::ExtendedRuntime => parent_cost.runtime = 0,
                    TenureChangeCause::ExtendedWriteCount => parent_cost.write_count = 0,
                    TenureChangeCause::ExtendedWriteLength => parent_cost.write_length = 0,

                    // These should be caught by the first match arm or are invalid here
                    TenureChangeCause::BlockFound | TenureChangeCause::Extended => {
                        panic!("Unexpected tenure change cause: {:?}", payload.cause);
                    }
                }
            }

            expected_total_tenure_cost
                .sub(&parent_cost)
                .expect("FATAL: failed to subtract parent total cost from self total cost");

            expected_total_tenure_cost
        }
    };

    let elected_height = sort_db
        .get_consensus_hash_height(&block.header.consensus_hash)?
        .ok_or_else(|| ChainstateError::NoSuchBlockError)?;
    let elected_in_cycle = sort_db
        .pox_constants
        .block_height_to_reward_cycle(sort_db.first_block_height, elected_height)
        .ok_or_else(|| {
            ChainstateError::InvalidStacksBlock(
                "Elected in block height before first_block_height".into(),
            )
        })?;
    let active_reward_set = OnChainRewardSetProvider::<DummyEventDispatcher>(None)
        .read_reward_set_nakamoto_of_cycle(
            elected_in_cycle,
            stacks_chain_state,
            sort_db,
            &block.header.parent_block_id,
            true,
        )
        .map_err(|e| {
            warn!(
                "Cannot process Nakamoto block: could not load reward set that elected the block";
                "err" => ?e,
                "consensus_hash" => %block.header.consensus_hash,
                "stacks_block_hash" => %block.header.block_hash(),
                "stacks_block_id" => %block.header.block_id(),
                "parent_block_id" => %block.header.parent_block_id,
            );
            ChainstateError::NoSuchBlockError
        })?;
    let (mut chainstate_tx, clarity_instance) = stacks_chain_state.chainstate_tx_begin()?;

    // find parent header
    let Some(parent_header_info) =
        NakamotoChainState::get_block_header(&chainstate_tx.tx, &block.header.parent_block_id)?
    else {
        // no parent; cannot process yet
        info!("Cannot process Nakamoto block: missing parent header";
               "consensus_hash" => %block.header.consensus_hash,
               "stacks_block_hash" => %block.header.block_hash(),
               "stacks_block_id" => %block.header.block_id(),
               "parent_block_id" => %block.header.parent_block_id
        );
        return Ok(());
    };

    // sanity check -- must attach to parent
    let parent_block_id = StacksBlockId::new(
        &parent_header_info.consensus_hash,
        &parent_header_info.anchored_header.block_hash(),
    );
    if parent_block_id != block.header.parent_block_id {
        drop(chainstate_tx);

        let msg = "Discontinuous Nakamoto Stacks block";
        warn!("{}", &msg;
              "child parent_block_id" => %block.header.parent_block_id,
              "expected parent_block_id" => %parent_block_id,
              "consensus_hash" => %block.header.consensus_hash,
              "stacks_block_hash" => %block.header.block_hash(),
              "stacks_block_id" => %block.header.block_id()
        );
        return Err(ChainstateError::InvalidStacksBlock(msg.into()));
    }

    // set the sortition handle's pointer to the block's burnchain view.
    //   this is either:
    //    (1)  set by the tenure change tx if one exists
    //    (2)  the same as parent block id

    let burnchain_view = if let Some(tenure_change) = block.get_tenure_tx_payload() {
        if let Some(ref parent_burn_view) = parent_header_info.burn_view {
            // check that the tenure_change's burn view descends from the parent
            let parent_burn_view_sn = SortitionDB::get_block_snapshot_consensus(
                sort_db.conn(),
                parent_burn_view,
            )?
            .ok_or_else(|| {
                warn!(
                    "Cannot process Nakamoto block: could not find parent block's burnchain view";
                    "consensus_hash" => %block.header.consensus_hash,
                    "stacks_block_hash" => %block.header.block_hash(),
                    "stacks_block_id" => %block.header.block_id(),
                    "parent_block_id" => %block.header.parent_block_id
                );
                ChainstateError::InvalidStacksBlock(
                    "Failed to load burn view of parent block ID".into(),
                )
            })?;
            let handle = sort_db.index_handle_at_ch(&tenure_change.burn_view_consensus_hash)?;
            let connected_sort_id = get_ancestor_sort_id(
                &handle,
                parent_burn_view_sn.block_height,
                &handle.context.chain_tip,
            )?
            .ok_or_else(|| {
                warn!(
                    "Cannot process Nakamoto block: could not find parent block's burnchain view";
                    "consensus_hash" => %block.header.consensus_hash,
                    "stacks_block_hash" => %block.header.block_hash(),
                    "stacks_block_id" => %block.header.block_id(),
                    "parent_block_id" => %block.header.parent_block_id
                );
                ChainstateError::InvalidStacksBlock(
                    "Failed to load burn view of parent block ID".into(),
                )
            })?;
            if connected_sort_id != parent_burn_view_sn.sortition_id {
                warn!(
                    "Cannot process Nakamoto block: parent block's burnchain view does not connect to own burn view";
                    "consensus_hash" => %block.header.consensus_hash,
                    "stacks_block_hash" => %block.header.block_hash(),
                    "stacks_block_id" => %block.header.block_id(),
                    "parent_block_id" => %block.header.parent_block_id
                );
                return Err(ChainstateError::InvalidStacksBlock(
                    "Does not connect to burn view of parent block ID".into(),
                ));
            }
        }
        &tenure_change.burn_view_consensus_hash
    } else {
        parent_header_info.burn_view.as_ref().ok_or_else(|| {
                warn!(
                    "Cannot process Nakamoto block: parent block does not have a burnchain view and current block has no tenure tx";
                    "consensus_hash" => %block.header.consensus_hash,
                    "stacks_block_hash" => %block.header.block_hash(),
                    "stacks_block_id" => %block.header.block_id(),
                    "parent_block_id" => %block.header.parent_block_id
                );
                ChainstateError::InvalidStacksBlock("Failed to load burn view of parent block ID".into())
            })?
    };
    let Some(burnchain_view_sn) =
        SortitionDB::get_block_snapshot_consensus(sort_db.conn(), burnchain_view)?
    else {
        // This should be checked already during block acceptance and parent block processing
        //   - The check for expected burns returns `NoSuchBlockError` if the burnchain view
        //      could not be found for a block with a tenure tx.
        // We error here anyways, but the check during block acceptance makes sure that the staging
        //  db doesn't get into a situation where it continuously tries to retry such a block (because
        //  such a block shouldn't land in the staging db).
        warn!(
            "Cannot process Nakamoto block: failed to find Sortition ID associated with burnchain view";
            "consensus_hash" => %block.header.consensus_hash,
            "stacks_block_hash" => %block.header.block_hash(),
            "stacks_block_id" => %block.header.block_id(),
            "burn_view_consensus_hash" => %burnchain_view,
        );
        return Ok(());
    };

    // find commit and sortition burns if this is a tenure-start block
    let new_tenure = block.is_wellformed_tenure_start_block()?;
    let (commit_burn, sortition_burn) = if new_tenure {
        // find block-commit to get commit-burn
        let block_commit = SortitionDB::get_block_commit(
            sort_db.conn(),
            &next_ready_block_snapshot.winning_block_txid,
            &next_ready_block_snapshot.sortition_id,
        )?
        .expect("FATAL: no block-commit for tenure-start block");

        let sort_burn =
            SortitionDB::get_block_burn_amount(sort_db.conn(), &next_ready_block_snapshot)?;
        (block_commit.burn_fee, sort_burn)
    } else {
        (0, 0)
    };

    // attach the block to the chain state and calculate the next chain tip.
    let pox_constants = sort_db.pox_constants.clone();

    // NOTE: because block status is updated in a separate transaction, we need `chainstate_tx`
    // and `clarity_instance` to go out of scope before we can issue the it (since we need a
    // mutable reference to `stacks_chain_state` to start it).  This means ensuring that, in the
    // `Ok(..)` case, the `clarity_commit` gets dropped beforehand.  In order to do this, we first
    // run `::append_block()` here, and capture both the Ok(..) and Err(..) results as
    // Option<..>'s.  Then, if we errored, we can explicitly drop the `Ok(..)` option (even
    // though it will always be None), which gets the borrow-checker to believe that it's safe
    // to access `stacks_chain_state` again.  In the `Ok(..)` case, it's instead sufficient so
    // simply commit the block before beginning the second transaction to mark it processed.
    let block_id = block.block_id();
    let mut burn_view_handle = sort_db.index_handle(&burnchain_view_sn.sortition_id);
    let (ok_opt, err_opt) = match NakamotoChainState::append_block(
        &mut chainstate_tx,
        clarity_instance,
        &mut burn_view_handle,
        burnchain_view,
        &pox_constants,
        &parent_header_info,
        &next_ready_block_snapshot.burn_header_hash,
        next_ready_block_snapshot
            .block_height
            .try_into()
            .expect("Failed to downcast u64 to u32"),
        next_ready_block_snapshot.burn_header_timestamp,
        block,
        block_size,
        commit_burn,
        sortition_burn,
        &active_reward_set,
        true,
    ) {
        Ok((receipt, _, _, _)) => (Some(receipt), None),
        Err(e) => (None, Some(e)),
    };

    if let Some(receipt) = ok_opt {
        // check the cost
        let evaluated_cost = receipt.anchored_block_cost.clone();
        if evaluated_cost != expected_cost {
            println!(
                "Failed processing block! block = {block_id}. Unexpected cost. expected = {expected_cost}, evaluated = {evaluated_cost}"
            );
            process::exit(1);
        }
    }

    if let Some(e) = err_opt {
        // force rollback
        drop(chainstate_tx);

        warn!(
            "Failed to append {}/{}: {:?}",
            &block.header.consensus_hash,
            &block.header.block_hash(),
            &e;
            "stacks_block_id" => %block.header.block_id()
        );

        // as a separate transaction, mark this block as processed and orphaned.
        // This is done separately so that the staging blocks DB, which receives writes
        // from the network to store blocks, will be available for writes while a block is
        // being processed. Therefore, it's *very important* that block-processing happens
        // within the same, single thread.  Also, it's *very important* that this update
        // succeeds, since *we have already processed* the block.
        return Err(e);
    };

    Ok(())
}

#[cfg(test)]
pub mod test {
    use super::*;

    fn parse_cli_command(s: &str) -> Vec<String> {
        s.split(' ').map(String::from).collect()
    }

    #[test]
    pub fn test_drain_common_opts() {
        // Should find/remove no options
        let mut argv = parse_cli_command(
            "stacks-inspect try-mine --config my_config.toml /tmp/chainstate/mainnet",
        );
        let argv_init = argv.clone();
        let _opts = drain_common_opts(&mut argv, 0);
        let opts = drain_common_opts(&mut argv, 1);

        assert_eq!(argv, argv_init);
        assert!(opts.config.is_none());

        // Should find config opts and remove from vec
        let mut argv = parse_cli_command(
            "stacks-inspect --network mocknet --network mainnet try-mine /tmp/chainstate/mainnet",
        );
        let opts = drain_common_opts(&mut argv, 1);
        let argv_expected = parse_cli_command("stacks-inspect try-mine /tmp/chainstate/mainnet");

        assert_eq!(argv, argv_expected);
        assert!(opts.config.is_some());
    }
}
