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

use std::collections::{BTreeMap, BTreeSet};

use clarity_types::representations::ClarityName;
use clarity_types::types::{PrincipalData, QualifiedContractIdentifier, Value};
use stacks_common::types::StacksEpochId;

use crate::vm::ClarityVersion;
use crate::vm::analysis::analysis_db::AnalysisDatabase;
use crate::vm::analysis::errors::{StaticCheckError, StaticCheckErrorKind};
use crate::vm::analysis::types::{
    AnalysisPass, AssetId, AssetOwnershipAccess, ChainStateRead, ContractAnalysis,
    ContractCall as ContractCallEffect, ContractReference, ContractStorageAccess, EffectTarget,
    FunctionEffects, PrincipalReference, Purity, StorageLocation, TokenKind,
};
use crate::vm::functions::NativeFunctions;
use crate::vm::functions::define::DefineFunctionsParsed;
use crate::vm::representations::SymbolicExpressionType::{
    Atom, AtomValue, Field, List, LiteralValue, TraitReference,
};
use crate::vm::representations::{SymbolicExpression, SymbolicExpressionType};

pub struct EffectsAnalyzer {
    /// Contract being analyzed.
    contract_identifier: QualifiedContractIdentifier,
    /// Clarity version for native function lookup.
    clarity_version: ClarityVersion,
    /// Per-function effect summaries for this contract.
    function_effects: BTreeMap<ClarityName, FunctionEffects>,
    /// Intra-contract call edges for effect propagation.
    /// Maps function names to the set of functions they call.
    call_graph: BTreeMap<ClarityName, BTreeSet<ClarityName>>,
    /// Function names declared in this contract.
    known_functions: BTreeSet<ClarityName>,
    /// Contract constants that resolve to literal/argument principals.
    principal_constants: BTreeMap<ClarityName, PrincipalReference>,
}

struct FunctionInfo {
    /// Function body expression.
    body: SymbolicExpression,
    /// Function parameter name -> argument index.
    param_indices: BTreeMap<ClarityName, u32>,
}

impl AnalysisPass for EffectsAnalyzer {
    fn run_pass(
        epoch: &StacksEpochId,
        contract_analysis: &mut ContractAnalysis,
        analysis_db: &mut AnalysisDatabase,
    ) -> Result<(), StaticCheckError> {
        let mut analyzer = EffectsAnalyzer::new(contract_analysis);
        analyzer.run(epoch, contract_analysis, analysis_db)
    }
}

impl EffectsAnalyzer {
    pub fn new(contract_analysis: &ContractAnalysis) -> Self {
        Self {
            contract_identifier: contract_analysis.contract_identifier.clone(),
            clarity_version: contract_analysis.clarity_version,
            function_effects: BTreeMap::new(),
            call_graph: BTreeMap::new(),
            known_functions: BTreeSet::new(),
            principal_constants: BTreeMap::new(),
        }
    }

    pub fn compute_deploy_effects(
        contract_analysis: &ContractAnalysis,
    ) -> Result<FunctionEffects, StaticCheckError> {
        let mut analyzer = EffectsAnalyzer::new(contract_analysis);
        analyzer.function_effects = contract_analysis.function_effects.clone();
        analyzer.known_functions = analyzer.function_effects.keys().cloned().collect();

        let mut deploy_effects = FunctionEffects::default();
        let param_indices = BTreeMap::new();
        let bindings = BTreeMap::new();
        let deploy_name = ClarityName::from("deploy");

        for expr in contract_analysis.expressions.iter() {
            if let Some(define_expr) = DefineFunctionsParsed::try_parse(expr)?
                && let DefineFunctionsParsed::Constant { name, value } = define_expr
            {
                let reference = analyzer.resolve_principal(value, &param_indices, &bindings);
                if reference != PrincipalReference::Any {
                    analyzer.principal_constants.insert(name.clone(), reference);
                }
            }
        }

        for expr in contract_analysis.expressions.iter() {
            if let Some(define_expr) = DefineFunctionsParsed::try_parse(expr)? {
                match define_expr {
                    DefineFunctionsParsed::Constant { value, .. } => {
                        analyzer.analyze_expr(
                            value,
                            &deploy_name,
                            &param_indices,
                            &bindings,
                            &mut deploy_effects,
                        );
                    }
                    DefineFunctionsParsed::PersistedVariable { name, initial, .. } => {
                        deploy_effects.writes.insert(EffectTarget::Contract(
                            ContractStorageAccess {
                                contract: ContractReference::Literal(
                                    analyzer.contract_identifier.clone(),
                                ),
                                location: Some(StorageLocation::DataVar(name.clone())),
                            },
                        ));
                        analyzer.analyze_expr(
                            initial,
                            &deploy_name,
                            &param_indices,
                            &bindings,
                            &mut deploy_effects,
                        );
                    }
                    DefineFunctionsParsed::Map { name, .. } => {
                        deploy_effects.writes.insert(EffectTarget::Contract(
                            ContractStorageAccess {
                                contract: ContractReference::Literal(
                                    analyzer.contract_identifier.clone(),
                                ),
                                location: Some(StorageLocation::DataMap(name.clone())),
                            },
                        ));
                    }
                    DefineFunctionsParsed::BoundedFungibleToken { max_supply, .. } => {
                        analyzer.analyze_expr(
                            max_supply,
                            &deploy_name,
                            &param_indices,
                            &bindings,
                            &mut deploy_effects,
                        );
                    }
                    _ => {}
                }
            } else {
                analyzer.analyze_expr(
                    expr,
                    &deploy_name,
                    &param_indices,
                    &bindings,
                    &mut deploy_effects,
                );
            }
        }

        analyzer
            .function_effects
            .insert(deploy_name.clone(), deploy_effects);
        analyzer.known_functions.insert(deploy_name.clone());
        analyzer.propagate_call_effects();
        let mut deploy_effects = analyzer
            .function_effects
            .remove(&deploy_name)
            .unwrap_or_default();
        deploy_effects.purity = if deploy_effects.reads.is_empty()
            && deploy_effects.writes.is_empty()
            && deploy_effects.contract_calls.is_empty()
        {
            Purity::Pure
        } else {
            Purity::Impure
        };
        Ok(deploy_effects)
    }

    pub fn run(
        &mut self,
        epoch: &StacksEpochId,
        contract_analysis: &mut ContractAnalysis,
        analysis_db: &mut AnalysisDatabase,
    ) -> Result<(), StaticCheckError> {
        // Collect function bodies and parameter indices for a single pass analysis.
        let mut function_bodies: BTreeMap<ClarityName, FunctionInfo> = BTreeMap::new();

        for expr in contract_analysis.expressions.iter() {
            if let Some(define_expr) = DefineFunctionsParsed::try_parse(expr)? {
                match define_expr {
                    DefineFunctionsParsed::Constant { name, value } => {
                        let reference =
                            self.resolve_principal(value, &BTreeMap::new(), &BTreeMap::new());
                        if reference != PrincipalReference::Any {
                            self.principal_constants.insert(name.clone(), reference);
                        }
                    }
                    DefineFunctionsParsed::PrivateFunction { signature, body }
                    | DefineFunctionsParsed::ReadOnlyFunction { signature, body }
                    | DefineFunctionsParsed::PublicFunction { signature, body } => {
                        let function_name = signature
                            .first()
                            .and_then(|name| name.match_atom())
                            .ok_or(StaticCheckErrorKind::DefineFunctionBadSignature)?;
                        self.function_effects
                            .entry(function_name.clone())
                            .or_default();
                        self.known_functions.insert(function_name.clone());
                        let param_indices = Self::extract_param_indices(signature);
                        function_bodies.insert(
                            function_name.clone(),
                            FunctionInfo {
                                body: body.clone(),
                                param_indices,
                            },
                        );
                    }
                    DefineFunctionsParsed::NonFungibleToken { .. }
                    | DefineFunctionsParsed::BoundedFungibleToken { .. }
                    | DefineFunctionsParsed::UnboundedFungibleToken { .. }
                    | DefineFunctionsParsed::Map { .. }
                    | DefineFunctionsParsed::PersistedVariable { .. }
                    | DefineFunctionsParsed::Trait { .. }
                    | DefineFunctionsParsed::UseTrait { .. }
                    | DefineFunctionsParsed::ImplTrait { .. } => {}
                }
            }
        }

        for (function_name, info) in function_bodies {
            let mut function_effects = self
                .function_effects
                .remove(&function_name)
                .unwrap_or_default();
            // Analyze each function body in isolation, then store its effects.
            self.analyze_expr(
                &info.body,
                &function_name,
                &info.param_indices,
                &BTreeMap::new(),
                &mut function_effects,
            );
            self.function_effects
                .insert(function_name.clone(), function_effects);
        }

        // Propagate effects across intra-contract calls.
        self.propagate_call_effects();
        // Resolve contract-call effects using any available callee contract analyses.
        self.resolve_contract_calls(epoch, analysis_db)?;
        // Derive purity from the final effect sets.
        self.update_purity();
        contract_analysis.function_effects = self.function_effects.clone();
        Ok(())
    }

    fn resolve_contract_calls(
        &mut self,
        epoch: &StacksEpochId,
        analysis_db: &mut AnalysisDatabase,
    ) -> Result<(), StaticCheckError> {
        // Build a transitive closure of contract effects we can load, starting from this contract.
        let contracts = self.load_contract_effects(epoch, analysis_db)?;
        let mut resolved_effects = BTreeMap::new();
        for (function, effects) in self.function_effects.iter() {
            let mut resolved = effects.clone();
            // Iteratively resolve contract calls until no further changes occur.
            loop {
                let next = resolved.resolve_contract_calls(&[], &contracts);
                if next == resolved {
                    break;
                }
                resolved = next;
            }
            resolved_effects.insert(function.clone(), resolved);
        }
        self.function_effects = resolved_effects;
        Ok(())
    }

    fn load_contract_effects(
        &self,
        epoch: &StacksEpochId,
        analysis_db: &mut AnalysisDatabase,
    ) -> Result<
        BTreeMap<QualifiedContractIdentifier, BTreeMap<ClarityName, FunctionEffects>>,
        StaticCheckError,
    > {
        // Seed the map with this contract's effects, then recursively load callees.
        let mut contracts = BTreeMap::new();
        contracts.insert(
            self.contract_identifier.clone(),
            self.function_effects.clone(),
        );
        let mut queue = vec![self.contract_identifier.clone()];
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
                if let Some(analysis) = analysis_db.load_contract(&callee, epoch)? {
                    contracts.insert(callee.clone(), analysis.function_effects);
                    queue.push(callee);
                }
            }
        }

        Ok(contracts)
    }

    fn update_purity(&mut self) {
        for effects in self.function_effects.values_mut() {
            let is_pure = effects.reads.is_empty()
                && effects.writes.is_empty()
                && effects.contract_calls.is_empty();
            effects.purity = if is_pure {
                Purity::Pure
            } else {
                Purity::Impure
            };
        }
    }

    fn propagate_call_effects(&mut self) {
        // Iteratively union callee effects into callers to reach a fixed point.
        let mut changed = true;
        while changed {
            changed = false;
            let snapshot = self.function_effects.clone();
            for (caller, callees) in self.call_graph.clone() {
                let Some(caller_effects) = self.function_effects.get_mut(&caller) else {
                    continue;
                };
                for callee in callees {
                    let Some(callee_effects) = snapshot.get(&callee) else {
                        continue;
                    };
                    for target in callee_effects.reads.iter().cloned() {
                        if caller_effects.reads.insert(target) {
                            changed = true;
                        }
                    }
                    for target in callee_effects.writes.iter().cloned() {
                        if caller_effects.writes.insert(target) {
                            changed = true;
                        }
                    }
                    for call in callee_effects.contract_calls.iter().cloned() {
                        if caller_effects.contract_calls.insert(call) {
                            changed = true;
                        }
                    }
                }
            }
        }
    }

    fn analyze_expr(
        &mut self,
        expr: &SymbolicExpression,
        current_function: &ClarityName,
        param_indices: &BTreeMap<ClarityName, u32>,
        bindings: &BTreeMap<ClarityName, PrincipalReference>,
        effects: &mut FunctionEffects,
    ) {
        // Walk the expression tree and aggregate direct effects.
        match &expr.expr {
            AtomValue(_) | LiteralValue(_) | Atom(_) | TraitReference(_, _) | Field(_) => {}
            List(expressions) => {
                self.analyze_list(
                    expressions,
                    current_function,
                    param_indices,
                    bindings,
                    effects,
                );
            }
        }
    }

    fn analyze_list(
        &mut self,
        expressions: &[SymbolicExpression],
        current_function: &ClarityName,
        param_indices: &BTreeMap<ClarityName, u32>,
        bindings: &BTreeMap<ClarityName, PrincipalReference>,
        effects: &mut FunctionEffects,
    ) {
        // Classify the head expression as native or user-defined call.
        let Some((function_name_expr, args)) = expressions.split_first() else {
            return;
        };
        let Some(function_name) = function_name_expr.match_atom() else {
            for expr in expressions {
                self.analyze_expr(expr, current_function, param_indices, bindings, effects);
            }
            return;
        };

        if let Some(native_function) =
            NativeFunctions::lookup_by_name_at_version(function_name, &self.clarity_version)
        {
            if native_function == NativeFunctions::Let {
                self.analyze_let(args, current_function, param_indices, bindings, effects);
                return;
            }
            for arg in args {
                self.analyze_expr(arg, current_function, param_indices, bindings, effects);
            }
            self.apply_native_effects(native_function, args, param_indices, bindings, effects);
        } else {
            for arg in args {
                self.analyze_expr(arg, current_function, param_indices, bindings, effects);
            }
            if self.known_functions.contains(function_name) {
                self.call_graph
                    .entry(current_function.clone())
                    .or_default()
                    .insert(function_name.clone());
            }
        }
    }

    fn apply_native_effects(
        &mut self,
        native_function: NativeFunctions,
        args: &[SymbolicExpression],
        param_indices: &BTreeMap<ClarityName, u32>,
        bindings: &BTreeMap<ClarityName, PrincipalReference>,
        effects: &mut FunctionEffects,
    ) {
        // Map native functions to the corresponding effect category.
        use NativeFunctions::*;
        match native_function {
            FetchVar => {
                if let Some(var_name) = args.first().and_then(|arg| arg.match_atom()) {
                    effects.reads.insert(self.contract_var_access(var_name));
                }
            }
            SetVar => {
                if let Some(var_name) = args.first().and_then(|arg| arg.match_atom()) {
                    effects.writes.insert(self.contract_var_access(var_name));
                }
            }
            FetchEntry => {
                if let Some(map_name) = args.first().and_then(|arg| arg.match_atom()) {
                    effects.reads.insert(self.contract_map_access(map_name));
                }
            }
            SetEntry | InsertEntry | DeleteEntry => {
                if let Some(map_name) = args.first().and_then(|arg| arg.match_atom()) {
                    effects.writes.insert(self.contract_map_access(map_name));
                }
            }
            GetBlockInfo => {
                effects
                    .reads
                    .insert(EffectTarget::ChainState(ChainStateRead::BlockInfo));
            }
            GetStacksBlockInfo => {
                effects
                    .reads
                    .insert(EffectTarget::ChainState(ChainStateRead::StacksBlockInfo));
            }
            GetBurnBlockInfo => {
                effects
                    .reads
                    .insert(EffectTarget::ChainState(ChainStateRead::BurnBlockInfo));
            }
            GetTenureInfo => {
                effects
                    .reads
                    .insert(EffectTarget::ChainState(ChainStateRead::TenureInfo));
            }
            GetStxBalance | StxGetAccount => {
                if let Some(principal) = args
                    .first()
                    .map(|expr| self.resolve_principal(expr, param_indices, bindings))
                {
                    effects
                        .reads
                        .insert(EffectTarget::AssetOwnership(AssetOwnershipAccess {
                            asset: AssetId::stx(),
                            principal,
                        }));
                }
            }
            StxTransfer | StxTransferMemo => {
                let from = args
                    .get(1)
                    .map(|expr| self.resolve_principal(expr, param_indices, bindings));
                let to = args
                    .get(2)
                    .map(|expr| self.resolve_principal(expr, param_indices, bindings));
                for principal in [from, to].into_iter().flatten() {
                    let access = EffectTarget::AssetOwnership(AssetOwnershipAccess {
                        asset: AssetId::stx(),
                        principal,
                    });
                    effects.reads.insert(access.clone());
                    effects.writes.insert(access);
                }
            }
            StxBurn => {
                if let Some(principal) = args
                    .get(1)
                    .map(|expr| self.resolve_principal(expr, param_indices, bindings))
                {
                    let access = EffectTarget::AssetOwnership(AssetOwnershipAccess {
                        asset: AssetId::stx(),
                        principal,
                    });
                    effects.reads.insert(access.clone());
                    effects.writes.insert(access);
                }
            }
            GetTokenBalance => {
                if let (Some(token_name), Some(principal_expr)) = (args.first(), args.get(1))
                    && let Some(asset) = self.ft_asset(token_name)
                {
                    let principal = self.resolve_principal(principal_expr, param_indices, bindings);
                    effects
                        .reads
                        .insert(EffectTarget::AssetOwnership(AssetOwnershipAccess {
                            asset,
                            principal,
                        }));
                }
            }
            GetTokenSupply => {
                if let Some(token_name) = args.first()
                    && let Some(asset) = self.ft_asset(token_name)
                {
                    effects
                        .reads
                        .insert(EffectTarget::AssetOwnership(AssetOwnershipAccess {
                            asset,
                            principal: PrincipalReference::Any,
                        }));
                }
            }
            TransferToken => {
                if let Some(asset) = args.first().and_then(|name| self.ft_asset(name)) {
                    let from = args
                        .get(2)
                        .map(|expr| self.resolve_principal(expr, param_indices, bindings));
                    let to = args
                        .get(3)
                        .map(|expr| self.resolve_principal(expr, param_indices, bindings));
                    for principal in [from, to].into_iter().flatten() {
                        let access = EffectTarget::AssetOwnership(AssetOwnershipAccess {
                            asset: asset.clone(),
                            principal,
                        });
                        effects.reads.insert(access.clone());
                        effects.writes.insert(access);
                    }
                }
            }
            MintToken => {
                if let Some(asset) = args.first().and_then(|name| self.ft_asset(name))
                    && let Some(principal) = args
                        .get(2)
                        .map(|expr| self.resolve_principal(expr, param_indices, bindings))
                {
                    let access =
                        EffectTarget::AssetOwnership(AssetOwnershipAccess { asset, principal });
                    effects.writes.insert(access);
                }
            }
            BurnToken => {
                if let Some(asset) = args.first().and_then(|name| self.ft_asset(name))
                    && let Some(principal) = args
                        .get(2)
                        .map(|expr| self.resolve_principal(expr, param_indices, bindings))
                {
                    let access =
                        EffectTarget::AssetOwnership(AssetOwnershipAccess { asset, principal });
                    effects.reads.insert(access.clone());
                    effects.writes.insert(access);
                }
            }
            GetAssetOwner => {
                if let Some(asset) = args.first().and_then(|name| self.nft_asset(name)) {
                    effects
                        .reads
                        .insert(EffectTarget::AssetOwnership(AssetOwnershipAccess {
                            asset,
                            principal: PrincipalReference::Any,
                        }));
                }
            }
            TransferAsset => {
                if let Some(asset) = args.first().and_then(|name| self.nft_asset(name)) {
                    let from = args
                        .get(2)
                        .map(|expr| self.resolve_principal(expr, param_indices, bindings));
                    let to = args
                        .get(3)
                        .map(|expr| self.resolve_principal(expr, param_indices, bindings));
                    for principal in [from, to].into_iter().flatten() {
                        let access = EffectTarget::AssetOwnership(AssetOwnershipAccess {
                            asset: asset.clone(),
                            principal,
                        });
                        effects.reads.insert(access.clone());
                        effects.writes.insert(access);
                    }
                }
            }
            MintAsset => {
                if let Some(asset) = args.first().and_then(|name| self.nft_asset(name))
                    && let Some(principal) = args
                        .get(2)
                        .map(|expr| self.resolve_principal(expr, param_indices, bindings))
                {
                    let access =
                        EffectTarget::AssetOwnership(AssetOwnershipAccess { asset, principal });
                    effects.writes.insert(access);
                }
            }
            BurnAsset => {
                if let Some(asset) = args.first().and_then(|name| self.nft_asset(name))
                    && let Some(principal) = args
                        .get(2)
                        .map(|expr| self.resolve_principal(expr, param_indices, bindings))
                {
                    let access =
                        EffectTarget::AssetOwnership(AssetOwnershipAccess { asset, principal });
                    effects.reads.insert(access.clone());
                    effects.writes.insert(access);
                }
            }
            ContractCall => {
                if let (Some(contract_expr), Some(function_name)) =
                    (args.first(), args.get(1).and_then(|expr| expr.match_atom()))
                {
                    let contract = match &contract_expr.expr {
                        SymbolicExpressionType::LiteralValue(Value::Principal(
                            PrincipalData::Contract(contract_identifier),
                        )) => ContractReference::Literal(contract_identifier.clone()),
                        SymbolicExpressionType::Atom(name) => param_indices
                            .get(name)
                            .copied()
                            .map(ContractReference::Argument)
                            .unwrap_or(ContractReference::Any),
                        _ => ContractReference::Any,
                    };
                    let arg_principals = args
                        .iter()
                        .skip(2)
                        .map(|expr| self.resolve_principal(expr, param_indices, bindings))
                        .collect();
                    effects.contract_calls.insert(ContractCallEffect {
                        contract,
                        function: function_name.clone(),
                        arg_principals,
                        caller: Some(self.contract_identifier.clone()),
                    });
                }
            }
            _ => {}
        }
    }

    fn resolve_principal(
        &self,
        expr: &SymbolicExpression,
        param_indices: &BTreeMap<ClarityName, u32>,
        bindings: &BTreeMap<ClarityName, PrincipalReference>,
    ) -> PrincipalReference {
        // Resolve literal principals, parameters, let-bindings, and constants.
        match &expr.expr {
            SymbolicExpressionType::LiteralValue(Value::Principal(principal)) => {
                PrincipalReference::Literal(principal.clone())
            }
            SymbolicExpressionType::Atom(name) => {
                if name.as_str() == "tx-sender" {
                    return PrincipalReference::TxSender;
                }
                if name.as_str() == "contract-caller" {
                    return PrincipalReference::ContractCaller;
                }
                if name.as_str() == "current-contract" {
                    return PrincipalReference::CurrentContract;
                }
                bindings
                    .get(name)
                    .cloned()
                    .or_else(|| {
                        param_indices
                            .get(name)
                            .copied()
                            .map(PrincipalReference::Argument)
                    })
                    .or_else(|| self.principal_constants.get(name).cloned())
                    .unwrap_or(PrincipalReference::Any)
            }
            SymbolicExpressionType::List(list) => {
                let Some((head, args)) = list.split_first() else {
                    return PrincipalReference::Any;
                };
                if head.match_atom().map(|name| name.as_str()) == Some("as-contract")
                    && let Some(inner) = args.first()
                    && let SymbolicExpressionType::Atom(atom) = &inner.expr
                    && atom.as_str() == "tx-sender"
                {
                    return PrincipalReference::CurrentContract;
                }
                PrincipalReference::Any
            }
            _ => PrincipalReference::Any,
        }
    }

    fn contract_map_access(&self, map_name: &ClarityName) -> EffectTarget {
        // Effects on a local data map are attributed to this contract.
        EffectTarget::Contract(ContractStorageAccess {
            contract: ContractReference::Literal(self.contract_identifier.clone()),
            location: Some(StorageLocation::DataMap(map_name.clone())),
        })
    }

    fn contract_var_access(&self, var_name: &ClarityName) -> EffectTarget {
        // Effects on a local data var are attributed to this contract.
        EffectTarget::Contract(ContractStorageAccess {
            contract: ContractReference::Literal(self.contract_identifier.clone()),
            location: Some(StorageLocation::DataVar(var_name.clone())),
        })
    }

    fn ft_asset(&self, token_name: &SymbolicExpression) -> Option<AssetId> {
        // Token identifiers are scoped to the current contract.
        token_name.match_atom().map(|name| {
            AssetId::token(
                self.contract_identifier.clone(),
                name.clone(),
                TokenKind::Fungible,
            )
        })
    }

    fn nft_asset(&self, token_name: &SymbolicExpression) -> Option<AssetId> {
        // Token identifiers are scoped to the current contract.
        token_name.match_atom().map(|name| {
            AssetId::token(
                self.contract_identifier.clone(),
                name.clone(),
                TokenKind::NonFungible,
            )
        })
    }

    fn analyze_let(
        &mut self,
        args: &[SymbolicExpression],
        current_function: &ClarityName,
        param_indices: &BTreeMap<ClarityName, u32>,
        bindings: &BTreeMap<ClarityName, PrincipalReference>,
        effects: &mut FunctionEffects,
    ) {
        // Evaluate bindings first, then analyze the let body with extended bindings.
        let mut extended = bindings.clone();
        let bindings_list = args
            .first()
            .and_then(|expr| expr.match_list())
            .unwrap_or(&[]);
        for binding in bindings_list {
            let Some(pair) = binding.match_list() else {
                continue;
            };
            let (Some(name), Some(value)) = (pair.first(), pair.get(1)) else {
                continue;
            };
            let Some(binding_name) = name.match_atom() else {
                continue;
            };
            self.analyze_expr(value, current_function, param_indices, bindings, effects);
            let reference = self.resolve_principal(value, param_indices, &extended);
            if reference != PrincipalReference::Any {
                extended.insert(binding_name.clone(), reference);
            }
        }

        for body_expr in args.iter().skip(1) {
            self.analyze_expr(
                body_expr,
                current_function,
                param_indices,
                &extended,
                effects,
            );
        }
    }

    fn extract_param_indices(signature: &[SymbolicExpression]) -> BTreeMap<ClarityName, u32> {
        // Map function parameters to their positional indices for argument resolution.
        let mut param_indices = BTreeMap::new();
        for (index, param) in signature.iter().skip(1).enumerate() {
            let Some(param_list) = param.match_list() else {
                continue;
            };
            let Some(param_name) = param_list.first().and_then(|name| name.match_atom()) else {
                continue;
            };
            param_indices.insert(param_name.clone(), index as u32);
        }
        param_indices
    }
}
