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

use std::collections::BTreeMap;

use stacks_common::types::StacksEpochId;

use crate::vm::ClarityVersion;
use crate::vm::analysis::effects_analyzer::EffectsAnalyzer;
use crate::vm::analysis::mem_type_check as mem_run_analysis;
use crate::vm::analysis::types::{
    AssetId, AssetOwnershipAccess, ChainStateRead, ContractCall, ContractReference,
    ContractStorageAccess, EffectTarget, FunctionEffects, PrincipalReference, Purity,
    StorageLocation, TokenKind,
};
use crate::vm::types::{PrincipalData, QualifiedContractIdentifier};

#[test]
fn test_effects_contract_call_argument_reference() {
    let snippet = "(define-trait compute-trait ((compute () (response uint uint))))
(define-public (do-it (computer <compute-trait>))
  (contract-call? computer compute)
)";

    let (_, analysis) =
        mem_run_analysis(snippet, ClarityVersion::latest(), StacksEpochId::latest()).unwrap();
    let effects = analysis
        .function_effects
        .get("do-it")
        .expect("missing function effects for do-it");
    let expected = ContractCall {
        contract: ContractReference::Argument(0),
        function: "compute".into(),
    };
    assert!(effects.contract_calls.contains(&expected));
}

#[test]
fn test_effects_contract_call_any_reference() {
    let snippet = "(define-trait compute-trait ((compute () (response uint uint))))
(define-public (do-it (computer <compute-trait>))
  (let ((alias computer))
    (contract-call? alias compute))
)";

    let (_, analysis) =
        mem_run_analysis(snippet, ClarityVersion::latest(), StacksEpochId::latest()).unwrap();
    let effects = analysis
        .function_effects
        .get("do-it")
        .expect("missing function effects for do-it");
    let expected = ContractCall {
        contract: ContractReference::Any,
        function: "compute".into(),
    };
    assert!(effects.contract_calls.contains(&expected));
}

#[test]
fn test_effects_map_and_var_access() {
    let snippet = "(define-data-var counter uint u0)
(define-map balances { owner: principal } { amount: uint })

(define-read-only (read-effects (owner principal))
  (begin
    (var-get counter)
    (map-get? balances { owner: owner })
    u1))

(define-public (write-effects (owner principal))
  (begin
    (var-set counter u1)
    (map-set balances { owner: owner } { amount: u1 })
    (map-insert balances { owner: owner } { amount: u2 })
    (map-delete balances { owner: owner })
    (ok true)))
";

    let (_, analysis) =
        mem_run_analysis(snippet, ClarityVersion::latest(), StacksEpochId::latest()).unwrap();
    let contract = ContractReference::Literal(analysis.contract_identifier.clone());
    let read_effects = analysis
        .function_effects
        .get("read-effects")
        .expect("missing function effects for read-effects");
    assert!(
        read_effects
            .reads
            .contains(&EffectTarget::Contract(ContractStorageAccess {
                contract: contract.clone(),
                location: Some(StorageLocation::DataVar("counter".into())),
            }))
    );
    assert!(
        read_effects
            .reads
            .contains(&EffectTarget::Contract(ContractStorageAccess {
                contract: contract.clone(),
                location: Some(StorageLocation::DataMap("balances".into())),
            }))
    );
    assert!(read_effects.writes.is_empty());

    let write_effects = analysis
        .function_effects
        .get("write-effects")
        .expect("missing function effects for write-effects");
    assert!(
        write_effects
            .writes
            .contains(&EffectTarget::Contract(ContractStorageAccess {
                contract: contract.clone(),
                location: Some(StorageLocation::DataVar("counter".into())),
            }))
    );
    assert!(
        write_effects
            .writes
            .contains(&EffectTarget::Contract(ContractStorageAccess {
                contract: contract.clone(),
                location: Some(StorageLocation::DataMap("balances".into())),
            }))
    );
}

#[test]
fn test_effects_deploy_top_level_expression() {
    let snippet = "(define-data-var counter uint u0)
(var-set counter u1)
";

    let (_, analysis) =
        mem_run_analysis(snippet, ClarityVersion::latest(), StacksEpochId::latest()).unwrap();
    let deploy_effects = EffectsAnalyzer::compute_deploy_effects(&analysis)
        .expect("deploy effects should analyze top-level expressions");
    let expected = EffectTarget::Contract(ContractStorageAccess {
        contract: ContractReference::Literal(analysis.contract_identifier.clone()),
        location: Some(StorageLocation::DataVar("counter".into())),
    });
    assert!(deploy_effects.writes.contains(&expected));
}

#[test]
fn test_effects_deploy_definitions_are_writes() {
    let snippet = "(define-data-var counter uint u0)
(define-map balances { owner: principal } { amount: uint })
";

    let (_, analysis) =
        mem_run_analysis(snippet, ClarityVersion::latest(), StacksEpochId::latest()).unwrap();
    let deploy_effects = EffectsAnalyzer::compute_deploy_effects(&analysis)
        .expect("deploy effects should include definition writes");
    let contract = ContractReference::Literal(analysis.contract_identifier.clone());
    assert!(
        deploy_effects
            .writes
            .contains(&EffectTarget::Contract(ContractStorageAccess {
                contract: contract.clone(),
                location: Some(StorageLocation::DataVar("counter".into())),
            }))
    );
    assert!(
        deploy_effects
            .writes
            .contains(&EffectTarget::Contract(ContractStorageAccess {
                contract,
                location: Some(StorageLocation::DataMap("balances".into())),
            }))
    );
}

#[test]
fn test_effects_chain_reads() {
    let snippet_v2 = "(define-read-only (chain-reads)
  (begin
    (get-block-info? time u1)
    (get-burn-block-info? header-hash u1)
    u1))
";

    let (_, analysis_v2) =
        mem_run_analysis(snippet_v2, ClarityVersion::Clarity2, StacksEpochId::Epoch22).unwrap();
    let effects_v2 = analysis_v2
        .function_effects
        .get("chain-reads")
        .expect("missing function effects for chain-reads");
    assert!(
        effects_v2
            .reads
            .contains(&EffectTarget::ChainState(ChainStateRead::BlockInfo))
    );
    assert!(
        effects_v2
            .reads
            .contains(&EffectTarget::ChainState(ChainStateRead::BurnBlockInfo))
    );

    let snippet_v3 = "(define-read-only (chain-reads)
  (begin
    (get-stacks-block-info? time u1)
    (get-tenure-info? time u1)
    u1))
";

    let (_, analysis_v3) =
        mem_run_analysis(snippet_v3, ClarityVersion::Clarity3, StacksEpochId::Epoch24).unwrap();
    let effects_v3 = analysis_v3
        .function_effects
        .get("chain-reads")
        .expect("missing function effects for chain-reads");
    assert!(
        effects_v3
            .reads
            .contains(&EffectTarget::ChainState(ChainStateRead::StacksBlockInfo))
    );
    assert!(
        effects_v3
            .reads
            .contains(&EffectTarget::ChainState(ChainStateRead::TenureInfo))
    );
}

#[test]
fn test_effects_assets_stx() {
    let snippet = "(define-read-only (stx-reads)
  (begin
    (stx-get-balance tx-sender)
    (stx-account tx-sender)
    u1))

(define-public (stx-writes)
  (begin
    (unwrap! (stx-transfer? u1 tx-sender contract-caller) (err u0))
    (unwrap! (stx-burn? u1 tx-sender) (err u0))
    (ok true)))
";

    let (_, analysis) =
        mem_run_analysis(snippet, ClarityVersion::latest(), StacksEpochId::latest()).unwrap();
    let expected = EffectTarget::AssetOwnership(AssetOwnershipAccess {
        asset: AssetId::stx(),
        principal: PrincipalReference::Any,
    });
    let reads = analysis
        .function_effects
        .get("stx-reads")
        .expect("missing function effects for stx-reads");
    assert!(reads.reads.contains(&expected));

    let writes = analysis
        .function_effects
        .get("stx-writes")
        .expect("missing function effects for stx-writes");
    assert!(writes.reads.contains(&expected));
    assert!(writes.writes.contains(&expected));
}

#[test]
fn test_effects_principal_binding_resolution() {
    let snippet = "(define-read-only (read-balance (p principal))
  (let ((q p))
    (stx-get-balance q)))
";

    let (_, analysis) =
        mem_run_analysis(snippet, ClarityVersion::latest(), StacksEpochId::latest()).unwrap();
    let expected = EffectTarget::AssetOwnership(AssetOwnershipAccess {
        asset: AssetId::stx(),
        principal: PrincipalReference::Argument(0),
    });
    let reads = analysis
        .function_effects
        .get("read-balance")
        .expect("missing function effects for read-balance");
    assert!(reads.reads.contains(&expected));
}

#[test]
fn test_effects_principal_constant_resolution() {
    let snippet = "(define-constant owner .callee)
(define-read-only (read-balance)
  (stx-get-balance owner))
";

    let (_, analysis) =
        mem_run_analysis(snippet, ClarityVersion::latest(), StacksEpochId::latest()).unwrap();
    let contract_id = QualifiedContractIdentifier::local("callee").unwrap();
    let expected = EffectTarget::AssetOwnership(AssetOwnershipAccess {
        asset: AssetId::stx(),
        principal: PrincipalReference::Literal(PrincipalData::Contract(contract_id)),
    });
    let reads = analysis
        .function_effects
        .get("read-balance")
        .expect("missing function effects for read-balance");
    assert!(reads.reads.contains(&expected));
}

#[test]
fn test_effects_assets_ft_nft() {
    let snippet = "(define-fungible-token token)
(define-non-fungible-token collectible uint)

(define-read-only (token-reads)
  (begin
    (ft-get-balance token tx-sender)
    (ft-get-supply token)
    (nft-get-owner? collectible u1)
    u1))

(define-public (token-writes)
  (begin
    (unwrap! (ft-transfer? token u1 tx-sender contract-caller) (err u0))
    (unwrap! (ft-mint? token u1 tx-sender) (err u0))
    (unwrap! (ft-burn? token u1 tx-sender) (err u0))
    (unwrap! (nft-transfer? collectible u1 tx-sender contract-caller) (err u0))
    (unwrap! (nft-mint? collectible u1 tx-sender) (err u0))
    (unwrap! (nft-burn? collectible u1 tx-sender) (err u0))
    (ok true)))
";

    let (_, analysis) =
        mem_run_analysis(snippet, ClarityVersion::latest(), StacksEpochId::latest()).unwrap();
    let ft_asset = AssetId::token(
        analysis.contract_identifier.clone(),
        "token".into(),
        TokenKind::Fungible,
    );
    let nft_asset = AssetId::token(
        analysis.contract_identifier.clone(),
        "collectible".into(),
        TokenKind::NonFungible,
    );
    let ft_access = EffectTarget::AssetOwnership(AssetOwnershipAccess {
        asset: ft_asset,
        principal: PrincipalReference::Any,
    });
    let nft_access = EffectTarget::AssetOwnership(AssetOwnershipAccess {
        asset: nft_asset,
        principal: PrincipalReference::Any,
    });

    let reads = analysis
        .function_effects
        .get("token-reads")
        .expect("missing function effects for token-reads");
    assert!(reads.reads.contains(&ft_access));
    assert!(reads.reads.contains(&nft_access));

    let writes = analysis
        .function_effects
        .get("token-writes")
        .expect("missing function effects for token-writes");
    assert!(writes.reads.contains(&ft_access));
    assert!(writes.reads.contains(&nft_access));
    assert!(writes.writes.contains(&ft_access));
    assert!(writes.writes.contains(&nft_access));
}

#[test]
fn test_effects_call_propagation_and_purity() {
    let snippet = "(define-data-var counter uint u0)

(define-private (inner)
  (begin
    (var-set counter u1)
    u1))

(define-read-only (pure)
  (+ u1 u2))

(define-public (outer)
  (begin
    (inner)
    (ok true)))
";

    let (_, analysis) =
        mem_run_analysis(snippet, ClarityVersion::latest(), StacksEpochId::latest()).unwrap();
    let contract = ContractReference::Literal(analysis.contract_identifier.clone());
    let write_effect = EffectTarget::Contract(ContractStorageAccess {
        contract,
        location: Some(StorageLocation::DataVar("counter".into())),
    });
    let outer = analysis
        .function_effects
        .get("outer")
        .expect("missing function effects for outer");
    assert!(outer.writes.contains(&write_effect));

    let pure = analysis
        .function_effects
        .get("pure")
        .expect("missing function effects for pure");
    assert_eq!(pure.purity, Purity::Pure);
}

#[test]
fn test_effects_contract_call_propagation() {
    let snippet = "(define-trait compute-trait ((compute () (response uint uint))))

(define-private (inner (computer <compute-trait>))
  (contract-call? computer compute))

(define-public (outer (computer <compute-trait>))
  (begin
    (unwrap! (inner computer) (err u0))
    (ok true)))
";

    let (_, analysis) =
        mem_run_analysis(snippet, ClarityVersion::latest(), StacksEpochId::latest()).unwrap();
    let expected = ContractCall {
        contract: ContractReference::Argument(0),
        function: "compute".into(),
    };
    let outer = analysis
        .function_effects
        .get("outer")
        .expect("missing function effects for outer");
    assert!(outer.contract_calls.contains(&expected));
}

#[test]
fn test_effects_contract_call_resolution() {
    let contract_id = QualifiedContractIdentifier::local("callee").unwrap();
    let mut callee_effects = FunctionEffects::default();
    callee_effects
        .reads
        .insert(EffectTarget::ChainState(ChainStateRead::StacksBlockInfo));

    let mut callee_functions = BTreeMap::new();
    callee_functions.insert("read".into(), callee_effects);
    let mut contracts = BTreeMap::new();
    contracts.insert(contract_id.clone(), callee_functions);

    let mut caller_effects = FunctionEffects::default();
    caller_effects.contract_calls.insert(ContractCall {
        contract: ContractReference::Literal(contract_id),
        function: "read".into(),
    });

    let resolved = caller_effects.resolve_contract_calls(&[], &contracts);
    assert!(
        resolved
            .reads
            .contains(&EffectTarget::ChainState(ChainStateRead::StacksBlockInfo))
    );
    assert!(resolved.contract_calls.is_empty());
}

#[test]
fn test_effects_contract_call_argument_resolution() {
    let contract_id = QualifiedContractIdentifier::local("callee").unwrap();
    let mut callee_effects = FunctionEffects::default();
    callee_effects
        .reads
        .insert(EffectTarget::ChainState(ChainStateRead::StacksBlockInfo));

    let mut callee_functions = BTreeMap::new();
    callee_functions.insert("read".into(), callee_effects);
    let mut contracts = BTreeMap::new();
    contracts.insert(contract_id.clone(), callee_functions);

    let mut caller_effects = FunctionEffects::default();
    caller_effects.contract_calls.insert(ContractCall {
        contract: ContractReference::Argument(0),
        function: "read".into(),
    });

    let resolved = caller_effects.resolve_contract_calls(&[Some(contract_id)], &contracts);
    assert!(
        resolved
            .reads
            .contains(&EffectTarget::ChainState(ChainStateRead::StacksBlockInfo))
    );
    assert!(resolved.contract_calls.is_empty());
}
