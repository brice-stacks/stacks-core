// Copyright (C) 2026 Stacks Open Internet Foundation
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

//! `audit-contract-types`: re-typecheck every contract of a chainstate and
//! report the type-checker behaviors that the Wasm runtime cannot reproduce.
//!
//! The type-checker is instrumented behind the `type-audit` feature of
//! `clarity-types` (see `clarity_types::audit`). This command enables the
//! instrumentation, re-runs the analysis of each contract exactly as it was
//! run at deploy time (same Clarity version, same epoch, dependencies read
//! from the chainstate at the chain tip), and collects what the hooks record.
//!
//! Two behaviors are currently tracked:
//! - tuple unification with mismatched key sets (stx-labs/clarity-wasm#858);
//! - `fold` result types that ignore the type of the initial value.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;

use clarity::vm::analysis::{AnalysisDatabase, run_analysis};
use clarity::vm::ast::build_ast;
use clarity::vm::clarity::ClarityConnection;
use clarity::vm::costs::LimitedCostTracker;
use clarity::vm::database::SqliteConnection;
use clarity::vm::resource_limiter::ResourceLimiter;
use clarity::vm::types::QualifiedContractIdentifier;
use clarity_types::audit::{self, TypeAuditEvent};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use stacks_common::types::chainstate::StacksBlockId;
use stackslib::chainstate::burn::db::sortdb::SortitionDB;
use stackslib::chainstate::nakamoto::NakamotoChainState;
use stackslib::chainstate::stacks::db::StacksChainState;

pub struct TypeAuditArgs {
    pub chain_tip: Option<StacksBlockId>,
    pub output: Option<String>,
    pub contracts: Vec<QualifiedContractIdentifier>,
}

impl TypeAuditArgs {
    pub fn parse(
        chain_tip: Option<String>,
        output: Option<String>,
        contracts: Vec<String>,
    ) -> Result<Self, String> {
        let chain_tip = chain_tip
            .map(|tip| StacksBlockId::from_hex(&tip).map_err(|e| format!("Bad chain tip: {e:?}")))
            .transpose()?;
        let contracts = contracts
            .iter()
            .map(|c| {
                QualifiedContractIdentifier::parse(c)
                    .map_err(|e| format!("Bad contract identifier '{c}': {e}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            chain_tip,
            output,
            contracts,
        })
    }
}

/// What the audit found in one contract.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Finding {
    /// `least_supertype` unified two tuple types with different key sets.
    TupleKeyMismatch {
        a: String,
        b: String,
        result: String,
    },
    /// The result type of a `fold` does not admit its initial value.
    FoldInitialType {
        initial: String,
        result: String,
        unified: Option<String>,
        line: u32,
        column: u32,
    },
    /// The stored source could not be found at the chain tip.
    MissingSource,
    /// The stored source no longer parses.
    ParseError(String),
    /// The stored source no longer type-checks.
    AnalysisError(String),
}

impl Finding {
    fn kind(&self) -> &'static str {
        match self {
            Finding::TupleKeyMismatch { .. } => "tuple-key-mismatch",
            Finding::FoldInitialType { .. } => "fold-initial-type",
            Finding::MissingSource => "missing-source",
            Finding::ParseError(_) => "parse-error",
            Finding::AnalysisError(_) => "analysis-error",
        }
    }

    fn from_event(event: TypeAuditEvent) -> Self {
        match event {
            TypeAuditEvent::TupleSupertypeKeyMismatch { a, b, result } => {
                Finding::TupleKeyMismatch {
                    a: a.to_string(),
                    b: b.to_string(),
                    result: result.to_string(),
                }
            }
            TypeAuditEvent::FoldInitialTypeMismatch {
                initial,
                result,
                unified,
                span,
            } => Finding::FoldInitialType {
                initial: initial.to_string(),
                result: result.to_string(),
                unified: unified.map(|t| t.to_string()),
                line: span.start_line,
                column: span.start_column,
            },
        }
    }

    fn to_json(&self) -> Value {
        match self {
            Finding::TupleKeyMismatch { a, b, result } => json!({
                "kind": self.kind(), "a": a, "b": b, "result": result,
            }),
            Finding::FoldInitialType {
                initial,
                result,
                unified,
                line,
                column,
            } => json!({
                "kind": self.kind(), "initial": initial, "result": result,
                "unified": unified, "line": line, "column": column,
            }),
            Finding::MissingSource => json!({ "kind": self.kind() }),
            Finding::ParseError(e) | Finding::AnalysisError(e) => {
                json!({ "kind": self.kind(), "error": e })
            }
        }
    }
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Finding::TupleKeyMismatch { a, b, result } => {
                write!(f, "tuple-key-mismatch: {a} vs {b} -> {result}")
            }
            Finding::FoldInitialType {
                initial,
                result,
                unified,
                line,
                column,
            } => {
                write!(
                    f,
                    "fold-initial-type at {line}:{column}: initial {initial}, result {result}, "
                )?;
                match unified {
                    Some(t) => write!(f, "should be {t}"),
                    None => write!(f, "cannot be unified"),
                }
            }
            Finding::MissingSource => write!(f, "missing-source"),
            Finding::ParseError(e) => write!(f, "parse-error: {e}"),
            Finding::AnalysisError(e) => write!(f, "analysis-error: {e}"),
        }
    }
}

struct ContractReport {
    contract: QualifiedContractIdentifier,
    clarity_version: String,
    epoch: String,
    findings: BTreeSet<Finding>,
}

/// Every contract identifier that has an analysis stored in the MARF side
/// store, whatever fork it was deployed on. Contracts not visible from the
/// chain tip are filtered out later, when their analysis fails to load.
fn list_contracts(marf_path: &str) -> Result<Vec<QualifiedContractIdentifier>, String> {
    let conn = Connection::open_with_flags(marf_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("Failed to open {marf_path}: {e}"))?;
    let pattern = format!("clr-meta::%::{}", AnalysisDatabase::storage_key());
    let mut stmt = conn
        .prepare("SELECT DISTINCT key FROM metadata_table WHERE key LIKE ?1")
        .map_err(|e| format!("Failed to query metadata_table: {e}"))?;
    let mut rows = stmt
        .query([pattern])
        .map_err(|e| format!("Failed to query metadata_table: {e}"))?;

    let mut contracts = BTreeSet::new();
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        let key: String = row.get(0).map_err(|e| e.to_string())?;
        let Some((contract_id, _)) = SqliteConnection::parse_metadata_key(&key) else {
            continue;
        };
        match QualifiedContractIdentifier::parse(contract_id) {
            Ok(id) => {
                contracts.insert(id);
            }
            Err(e) => eprintln!("Skipping unparseable contract id '{contract_id}': {e}"),
        }
    }
    Ok(contracts.into_iter().collect())
}

pub fn command_audit_contract_types(
    sort_db: &SortitionDB,
    chain_state: &mut StacksChainState,
    args: TypeAuditArgs,
) -> Result<(), String> {
    let chain_tip = match args.chain_tip {
        Some(tip) => tip,
        None => NakamotoChainState::get_canonical_block_header(chain_state.db(), sort_db)
            .map_err(|e| format!("Failed to load canonical tip: {e:?}"))?
            .ok_or_else(|| "No canonical chain tip".to_string())?
            .index_block_hash(),
    };

    let contracts = if args.contracts.is_empty() {
        list_contracts(&chain_state.clarity_state_index_path)?
    } else {
        args.contracts.clone()
    };
    eprintln!(
        "Auditing {} contract(s) at chain tip {chain_tip}",
        contracts.len()
    );

    audit::enable();
    let burn_dbconn = sort_db.index_handle_at_tip();
    let (reports, not_at_tip) = chain_state
        .with_read_only_clarity_tx(&burn_dbconn, &chain_tip, |conn| {
            let mut reports = Vec::new();
            let mut not_at_tip = Vec::new();
            for (i, contract) in contracts.iter().enumerate() {
                if i > 0 && i % 500 == 0 {
                    eprintln!("  {i}/{} contracts audited", contracts.len());
                }
                match audit_contract(conn, contract) {
                    Some(report) => {
                        if !report.findings.is_empty() {
                            reports.push(report);
                        }
                    }
                    None => not_at_tip.push(contract.clone()),
                }
            }
            (reports, not_at_tip)
        })
        .ok_or_else(|| format!("Chain tip {chain_tip} is not a known block"))?;
    audit::disable();

    print_report(
        &chain_tip,
        contracts.len(),
        &not_at_tip,
        &reports,
        args.output.as_deref(),
    )
}

/// Re-run the analysis of one contract and collect what the audit hooks record.
/// Returns `None` when the contract is not visible from the chain tip.
fn audit_contract(
    conn: &mut impl ClarityConnection,
    contract: &QualifiedContractIdentifier,
) -> Option<ContractReport> {
    let (analysis, source) = conn.with_clarity_db_readonly(|db| {
        let analysis = db.load_contract_analysis(contract);
        let source = db.get_contract_src(contract);
        (analysis, source)
    });

    let analysis = match analysis {
        Ok(Some(analysis)) => analysis,
        Ok(None) => return None,
        Err(e) => {
            eprintln!("Failed to load the analysis of {contract}: {e:?}");
            return None;
        }
    };
    let epoch = analysis.epoch;
    let clarity_version = analysis.clarity_version;
    let mut report = ContractReport {
        contract: contract.clone(),
        clarity_version: clarity_version.to_string(),
        epoch: format!("{epoch:?}"),
        findings: BTreeSet::new(),
    };

    let Some(source) = source else {
        report.findings.insert(Finding::MissingSource);
        return Some(report);
    };

    let ast = match build_ast(
        contract,
        &source,
        &mut LimitedCostTracker::new_free(),
        clarity_version,
        epoch,
    ) {
        Ok(ast) => ast,
        Err(e) => {
            report
                .findings
                .insert(Finding::ParseError(format!("{e:?}")));
            return Some(report);
        }
    };

    // Discard anything recorded while loading dependencies.
    audit::drain();
    let result = conn.with_analysis_db_readonly(|analysis_db| {
        run_analysis(
            contract,
            &ast.expressions,
            analysis_db,
            false,
            LimitedCostTracker::new_free(),
            epoch,
            clarity_version,
            false,
            ResourceLimiter::unlimited(),
        )
    });
    for event in audit::drain() {
        report.findings.insert(Finding::from_event(event));
    }
    if let Err(e) = result {
        report
            .findings
            .insert(Finding::AnalysisError(e.0.to_string()));
    }

    Some(report)
}

fn print_report(
    chain_tip: &StacksBlockId,
    scanned: usize,
    not_at_tip: &[QualifiedContractIdentifier],
    reports: &[ContractReport],
    output: Option<&str>,
) -> Result<(), String> {
    let mut counts: BTreeMap<&'static str, BTreeSet<&QualifiedContractIdentifier>> =
        BTreeMap::new();
    for report in reports {
        for finding in &report.findings {
            counts
                .entry(finding.kind())
                .or_default()
                .insert(&report.contract);
        }
    }

    println!("Chain tip: {chain_tip}");
    println!("Contracts found in the side store: {scanned}");
    println!(
        "Contracts not visible from the chain tip: {}",
        not_at_tip.len()
    );
    println!(
        "Contracts audited: {}",
        scanned.saturating_sub(not_at_tip.len())
    );
    println!("Contracts with findings: {}", reports.len());
    for (kind, contracts) in &counts {
        println!("  {kind}: {} contract(s)", contracts.len());
    }
    for report in reports {
        println!(
            "\n{} ({}, {}):",
            report.contract, report.clarity_version, report.epoch
        );
        for finding in &report.findings {
            println!("  {finding}");
        }
    }

    if let Some(path) = output {
        let json = json!({
            "chain_tip": chain_tip.to_string(),
            "contracts_found": scanned,
            "contracts_not_at_tip": not_at_tip.iter().map(|c| c.to_string()).collect::<Vec<_>>(),
            "summary": counts.iter().map(|(kind, contracts)| {
                (kind.to_string(), json!(contracts.iter().map(|c| c.to_string()).collect::<Vec<_>>()))
            }).collect::<serde_json::Map<_, _>>(),
            "contracts": reports.iter().map(|r| json!({
                "contract": r.contract.to_string(),
                "clarity_version": r.clarity_version,
                "epoch": r.epoch,
                "findings": r.findings.iter().map(Finding::to_json).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        });
        let mut file =
            fs::File::create(path).map_err(|e| format!("Failed to create {path}: {e}"))?;
        file.write_all(serde_json::to_string_pretty(&json).unwrap().as_bytes())
            .map_err(|e| format!("Failed to write {path}: {e}"))?;
        println!("\nFull report written to {path}");
    }
    Ok(())
}
