//! Candidate selection. See the [rustc dev guide] for more information on how this works.
//!
//! [rustc dev guide]: https://rustc-dev-guide.rust-lang.org/traits/resolution.html#selection

use self::EvaluationResult::*;
use self::SelectionCandidate::*;

use super::coherence::{self, Conflict};
use super::const_evaluatable;
use super::project;
use super::project::ProjectionTermObligation;
use super::util;
use super::util::closure_trait_ref_and_return_type;
use super::wf;
use super::{
    ImplDerivedCause, Normalized, Obligation, ObligationCause, ObligationCauseCode, Overflow,
    PolyTraitObligation, PredicateObligation, Selection, SelectionError, SelectionResult,
    TraitQueryMode,
};

use crate::error_reporting::traits::TypeErrCtxtExt;
use crate::infer::{InferCtxt, InferCtxtExt, InferOk, TypeFreshener};
use crate::solve::InferCtxtSelectExt as _;
use crate::traits::normalize::normalize_with_depth;
use crate::traits::normalize::normalize_with_depth_to;
use crate::traits::project::ProjectAndUnifyResult;
use crate::traits::project::ProjectionCacheKeyExt;
use crate::traits::ProjectionCacheKey;
use crate::traits::Unimplemented;
use rustc_data_structures::fx::{FxHashSet, FxIndexMap, FxIndexSet};
use rustc_data_structures::stack::ensure_sufficient_stack;
use rustc_errors::{Diag, EmissionGuarantee};
use rustc_hir as hir;
use rustc_hir::def_id::DefId;
use rustc_hir::LangItem;
use rustc_infer::infer::relate::TypeRelation;
use rustc_infer::infer::BoundRegionConversionTime;
use rustc_infer::infer::BoundRegionConversionTime::HigherRankedType;
use rustc_infer::infer::DefineOpaqueTypes;
use rustc_infer::traits::TraitObligation;
use rustc_middle::bug;
use rustc_middle::dep_graph::dep_kinds;
use rustc_middle::dep_graph::DepNodeIndex;
use rustc_middle::mir::interpret::ErrorHandled;
use rustc_middle::ty::abstract_const::NotConstEvaluatable;
use rustc_middle::ty::error::TypeErrorToStringExt;
use rustc_middle::ty::print::PrintTraitRefExt as _;
use rustc_middle::ty::GenericArgsRef;
use rustc_middle::ty::{self, PolyProjectionPredicate, Upcast};
use rustc_middle::ty::{Ty, TyCtxt, TypeFoldable, TypeVisitableExt};
use rustc_span::symbol::sym;
use rustc_span::Symbol;

use std::cell::{Cell, RefCell};
use std::cmp;
use std::fmt::{self, Display};
use std::iter;
use std::ops::ControlFlow;

pub use rustc_middle::traits::select::*;
use rustc_middle::ty::print::with_no_trimmed_paths;

mod _match;
mod candidate_assembly;
mod confirmation;

/// Whether to consider the binder of higher ranked goals for the `leak_check` when
/// evaluating higher-ranked goals. See #119820 for more info.
///
/// While this is a bit hacky, it is necessary to match the behavior of the new solver:
/// We eagerly instantiate binders in the new solver, outside of candidate selection, so
/// the leak check inside of candidates does not consider any bound vars from the higher
/// ranked goal. However, we do exit the binder once we're completely finished with a goal,
/// so the leak-check can be used in evaluate by causing nested higher-ranked goals to fail.
#[derive(Debug, Copy, Clone)]
enum LeakCheckHigherRankedGoal {
    No,
    Yes,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum IntercrateAmbiguityCause<'tcx> {
    DownstreamCrate { trait_ref: ty::TraitRef<'tcx>, self_ty: Option<Ty<'tcx>> },
    UpstreamCrateUpdate { trait_ref: ty::TraitRef<'tcx>, self_ty: Option<Ty<'tcx>> },
    ReservationImpl { message: Symbol },
}

impl<'tcx> IntercrateAmbiguityCause<'tcx> {
    /// Emits notes when the overlap is caused by complex intercrate ambiguities.
    /// See #23980 for details.
    pub fn add_intercrate_ambiguity_hint<G: EmissionGuarantee>(&self, err: &mut Diag<'_, G>) {
        err.note(self.intercrate_ambiguity_hint());
    }

    pub fn intercrate_ambiguity_hint(&self) -> String {
        with_no_trimmed_paths!(match self {
            IntercrateAmbiguityCause::DownstreamCrate { trait_ref, self_ty } => {
                format!(
                    "downstream crates may implement trait `{trait_desc}`{self_desc}",
                    trait_desc = trait_ref.print_trait_sugared(),
                    self_desc = if let Some(self_ty) = self_ty {
                        format!(" for type `{self_ty}`")
                    } else {
                        String::new()
                    }
                )
            }
            IntercrateAmbiguityCause::UpstreamCrateUpdate { trait_ref, self_ty } => {
                format!(
                    "upstream crates may add a new impl of trait `{trait_desc}`{self_desc} \
                in future versions",
                    trait_desc = trait_ref.print_trait_sugared(),
                    self_desc = if let Some(self_ty) = self_ty {
                        format!(" for type `{self_ty}`")
                    } else {
                        String::new()
                    }
                )
            }
            IntercrateAmbiguityCause::ReservationImpl { message } => message.to_string(),
        })
    }
}

pub struct SelectionContext<'cx, 'tcx> {
    pub infcx: &'cx InferCtxt<'tcx>,

    /// Freshener used specifically for entries on the obligation
    /// stack. This ensures that all entries on the stack at one time
    /// will have the same set of placeholder entries, which is
    /// important for checking for trait bounds that recursively
    /// require themselves.
    freshener: TypeFreshener<'cx, 'tcx>,

    /// If `intercrate` is set, we remember predicates which were
    /// considered ambiguous because of impls potentially added in other crates.
    /// This is used in coherence to give improved diagnostics.
    /// We don't do his until we detect a coherence error because it can
    /// lead to false overflow results (#47139) and because always
    /// computing it may negatively impact performance.
    intercrate_ambiguity_causes: Option<FxIndexSet<IntercrateAmbiguityCause<'tcx>>>,

    /// The mode that trait queries run in, which informs our error handling
    /// policy. In essence, canonicalized queries need their errors propagated
    /// rather than immediately reported because we do not have accurate spans.
    query_mode: TraitQueryMode,
}

// A stack that walks back up the stack frame.
struct TraitObligationStack<'prev, 'tcx> {
    obligation: &'prev PolyTraitObligation<'tcx>,

    /// The trait predicate from `obligation` but "freshened" with the
    /// selection-context's freshener. Used to check for recursion.
    fresh_trait_pred: ty::PolyTraitPredicate<'tcx>,

    /// Starts out equal to `depth` -- if, during evaluation, we
    /// encounter a cycle, then we will set this flag to the minimum
    /// depth of that cycle for all participants in the cycle. These
    /// participants will then forego caching their results. This is
    /// not the most efficient solution, but it addresses #60010. The
    /// problem we are trying to prevent:
    ///
    /// - If you have `A: AutoTrait` requires `B: AutoTrait` and `C: NonAutoTrait`
    /// - `B: AutoTrait` requires `A: AutoTrait` (coinductive cycle, ok)
    /// - `C: NonAutoTrait` requires `A: AutoTrait` (non-coinductive cycle, not ok)
    ///
    /// you don't want to cache that `B: AutoTrait` or `A: AutoTrait`
    /// is `EvaluatedToOk`; this is because they were only considered
    /// ok on the premise that if `A: AutoTrait` held, but we indeed
    /// encountered a problem (later on) with `A: AutoTrait`. So we
    /// currently set a flag on the stack node for `B: AutoTrait` (as
    /// well as the second instance of `A: AutoTrait`) to suppress
    /// caching.
    ///
    /// This is a simple, targeted fix. A more-performant fix requires
    /// deeper changes, but would permit more caching: we could
    /// basically defer caching until we have fully evaluated the
    /// tree, and then cache the entire tree at once. In any case, the
    /// performance impact here shouldn't be so horrible: every time
    /// this is hit, we do cache at least one trait, so we only
    /// evaluate each member of a cycle up to N times, where N is the
    /// length of the cycle. This means the performance impact is
    /// bounded and we shouldn't have any terrible worst-cases.
    reached_depth: Cell<usize>,

    previous: TraitObligationStackList<'prev, 'tcx>,

    /// The number of parent frames plus one (thus, the topmost frame has depth 1).
    depth: usize,

    /// The depth-first number of this node in the search graph -- a
    /// pre-order index. Basically, a freshly incremented counter.
    dfn: usize,
}

struct SelectionCandidateSet<'tcx> {
    /// A list of candidates that definitely apply to the current
    /// obligation (meaning: types unify).
    vec: Vec<SelectionCandidate<'tcx>>,

    /// If `true`, then there were candidates that might or might
    /// not have applied, but we couldn't tell. This occurs when some
    /// of the input types are type variables, in which case there are
    /// various "builtin" rules that might or might not trigger.
    ambiguous: bool,
}

#[derive(PartialEq, Eq, Debug, Clone)]
struct EvaluatedCandidate<'tcx> {
    candidate: SelectionCandidate<'tcx>,
    evaluation: EvaluationResult,
}

/// When does the builtin impl for `T: Trait` apply?
#[derive(Debug)]
enum BuiltinImplConditions<'tcx> {
    /// The impl is conditional on `T1, T2, ...: Trait`.
    Where(ty::Binder<'tcx, Vec<Ty<'tcx>>>),
    /// There is no built-in impl. There may be some other
    /// candidate (a where-clause or user-defined impl).
    None,
    /// It is unknown whether there is an impl.
    Ambiguous,
}

impl<'cx, 'tcx> SelectionContext<'cx, 'tcx> {
    pub fn new(infcx: &'cx InferCtxt<'tcx>) -> SelectionContext<'cx, 'tcx> {
        SelectionContext {
            infcx,
            freshener: infcx.freshener(),
            intercrate_ambiguity_causes: None,
            query_mode: TraitQueryMode::Standard,
        }
    }

    pub fn with_query_mode(
        infcx: &'cx InferCtxt<'tcx>,
        query_mode: TraitQueryMode,
    ) -> SelectionContext<'cx, 'tcx> {
        debug!(?query_mode, "with_query_mode");
        SelectionContext { query_mode, ..SelectionContext::new(infcx) }
    }

    /// Enables tracking of intercrate ambiguity causes. See
    /// the documentation of [`Self::intercrate_ambiguity_causes`] for more.
    pub fn enable_tracking_intercrate_ambiguity_causes(&mut self) {
        assert!(self.is_intercrate());
        assert!(self.intercrate_ambiguity_causes.is_none());
        self.intercrate_ambiguity_causes = Some(FxIndexSet::default());
        debug!("selcx: enable_tracking_intercrate_ambiguity_causes");
    }

    /// Gets the intercrate ambiguity causes collected since tracking
    /// was enabled and disables tracking at the same time. If
    /// tracking is not enabled, just returns an empty vector.
    pub fn take_intercrate_ambiguity_causes(
        &mut self,
    ) -> FxIndexSet<IntercrateAmbiguityCause<'tcx>> {
        assert!(self.is_intercrate());
        self.intercrate_ambiguity_causes.take().unwrap_or_default()
    }

    pub fn tcx(&self) -> TyCtxt<'tcx> {
        self.infcx.tcx
    }

    pub fn is_intercrate(&self) -> bool {
        self.infcx.intercrate
    }

    ///////////////////////////////////////////////////////////////////////////
    // Selection
    //
    // The selection phase tries to identify *how* an obligation will
    // be resolved. For example, it will identify which impl or
    // parameter bound is to be used. The process can be inconclusive
    // if the self type in the obligation is not fully inferred. Selection
    // can result in an error in one of two ways:
    //
    // 1. If no applicable impl or parameter bound can be found.
    // 2. If the output type parameters in the obligation do not match
    //    those specified by the impl/bound. For example, if the obligation
    //    is `Vec<Foo>: Iterable<Bar>`, but the impl specifies
    //    `impl<T> Iterable<T> for Vec<T>`, than an error would result.

    /// Attempts to satisfy the obligation. If successful, this will affect the surrounding
    /// type environment by performing unification.
    #[instrument(level = "debug", skip(self), ret)]
    pub fn poly_select(
        &mut self,
        obligation: &PolyTraitObligation<'tcx>,
    ) -> SelectionResult<'tcx, Selection<'tcx>> {
        if self.infcx.next_trait_solver() {
            return self.infcx.select_in_new_trait_solver(obligation);
        }

        let candidate = match self.select_from_obligation(obligation) {
            Err(SelectionError::Overflow(OverflowError::Canonical)) => {
                // In standard mode, overflow must have been caught and reported
                // earlier.
                assert!(self.query_mode == TraitQueryMode::Canonical);
                return Err(SelectionError::Overflow(OverflowError::Canonical));
            }
            Err(e) => {
                return Err(e);
            }
            Ok(None) => {
                return Ok(None);
            }
            Ok(Some(candidate)) => candidate,
        };

        match self.confirm_candidate(obligation, candidate) {
            Err(SelectionError::Overflow(OverflowError::Canonical)) => {
                assert!(self.query_mode == TraitQueryMode::Canonical);
                Err(SelectionError::Overflow(OverflowError::Canonical))
            }
            Err(e) => Err(e),
            Ok(candidate) => Ok(Some(candidate)),
        }
    }

    pub fn select(
        &mut self,
        obligation: &TraitObligation<'tcx>,
    ) -> SelectionResult<'tcx, Selection<'tcx>> {
        self.poly_select(&Obligation {
            cause: obligation.cause.clone(),
            param_env: obligation.param_env,
            predicate: ty::Binder::dummy(obligation.predicate),
            recursion_depth: obligation.recursion_depth,
        })
    }

    fn select_from_obligation(
        &mut self,
        obligation: &PolyTraitObligation<'tcx>,
    ) -> SelectionResult<'tcx, SelectionCandidate<'tcx>> {
        debug_assert!(!obligation.predicate.has_escaping_bound_vars());

        let pec = &ProvisionalEvaluationCache::default();
        let stack = self.push_stack(TraitObligationStackList::empty(pec), obligation);

        self.candidate_from_obligation(&stack)
    }

    #[instrument(level = "debug", skip(self), ret)]
    fn candidate_from_obligation<'o>(
        &mut self,
        stack: &TraitObligationStack<'o, 'tcx>,
    ) -> SelectionResult<'tcx, SelectionCandidate<'tcx>> {
        debug_assert!(!self.infcx.next_trait_solver());
        // Watch out for overflow. This intentionally bypasses (and does
        // not update) the cache.
        self.check_recursion_limit(stack.obligation, stack.obligation)?;

        // Check the cache. Note that we freshen the trait-ref
        // separately rather than using `stack.fresh_trait_ref` --
        // this is because we want the unbound variables to be
        // replaced with fresh types starting from index 0.
        let cache_fresh_trait_pred = self.infcx.freshen(stack.obligation.predicate);
        debug!(?cache_fresh_trait_pred);
        debug_assert!(!stack.obligation.predicate.has_escaping_bound_vars());

        if let Some(c) =
            self.check_candidate_cache(stack.obligation.param_env, cache_fresh_trait_pred)
        {
            debug!("CACHE HIT");
            return c;
        }

        // If no match, compute result and insert into cache.
        //
        // FIXME(nikomatsakis) -- this cache is not taking into
        // account cycles that may have occurred in forming the
        // candidate. I don't know of any specific problems that
        // result but it seems awfully suspicious.
        let (candidate, dep_node) =
            self.in_task(|this| this.candidate_from_obligation_no_cache(stack));

        debug!("CACHE MISS");
        self.insert_candidate_cache(
            stack.obligation.param_env,
            cache_fresh_trait_pred,
            dep_node,
            candidate.clone(),
        );
        candidate
    }

    fn candidate_from_obligation_no_cache<'o>(
        &mut self,
        stack: &TraitObligationStack<'o, 'tcx>,
    ) -> SelectionResult<'tcx, SelectionCandidate<'tcx>> {
        if let Err(conflict) = self.is_knowable(stack) {
            debug!("coherence stage: not knowable");
            if self.intercrate_ambiguity_causes.is_some() {
                debug!("evaluate_stack: intercrate_ambiguity_causes is some");
                // Heuristics: show the diagnostics when there are no candidates in crate.
                if let Ok(candidate_set) = self.assemble_candidates(stack) {
                    let mut no_candidates_apply = true;

                    for c in candidate_set.vec.iter() {
                        if self
                            .evaluate_candidate(stack, c, LeakCheckHigherRankedGoal::No)?
                            .may_apply()
                        {
                            no_candidates_apply = false;
                            break;
                        }
                    }

                    if !candidate_set.ambiguous && no_candidates_apply {
                        let trait_ref = self.infcx.resolve_vars_if_possible(
                            stack.obligation.predicate.skip_binder().trait_ref,
                        );
                        if !trait_ref.references_error() {
                            let self_ty = trait_ref.self_ty();
                            let self_ty = self_ty.has_concrete_skeleton().then(|| self_ty);
                            let cause = if let Conflict::Upstream = conflict {
                                IntercrateAmbiguityCause::UpstreamCrateUpdate { trait_ref, self_ty }
                            } else {
                                IntercrateAmbiguityCause::DownstreamCrate { trait_ref, self_ty }
                            };
                            debug!(?cause, "evaluate_stack: pushing cause");
                            self.intercrate_ambiguity_causes.as_mut().unwrap().insert(cause);
                        }
                    }
                }
            }
            return Ok(None);
        }

        let candidate_set = self.assemble_candidates(stack)?;

        if candidate_set.ambiguous {
            debug!("candidate set contains ambig");
            return Ok(None);
        }

        let candidates = candidate_set.vec;

        debug!(?stack, ?candidates, "assembled {} candidates", candidates.len());

        // At this point, we know that each of the entries in the
        // candidate set is *individually* applicable. Now we have to
        // figure out if they contain mutual incompatibilities. This
        // frequently arises if we have an unconstrained input type --
        // for example, we are looking for `$0: Eq` where `$0` is some
        // unconstrained type variable. In that case, we'll get a
        // candidate which assumes $0 == int, one that assumes `$0 ==
        // usize`, etc. This spells an ambiguity.

        let mut candidates = self.filter_impls(candidates, stack.obligation);

        // If there is more than one candidate, first winnow them down
        // by considering extra conditions (nested obligations and so
        // forth). We don't winnow if there is exactly one
        // candidate. This is a relatively minor distinction but it
        // can lead to better inference and error-reporting. An
        // example would be if there was an impl:
        //
        //     impl<T:Clone> Vec<T> { fn push_clone(...) { ... } }
        //
        // and we were to see some code `foo.push_clone()` where `boo`
        // is a `Vec<Bar>` and `Bar` does not implement `Clone`. If
        // we were to winnow, we'd wind up with zero candidates.
        // Instead, we select the right impl now but report "`Bar` does
        // not implement `Clone`".
        if candidates.len() == 1 {
            return self.filter_reservation_impls(candidates.pop().unwrap());
        }

        // Winnow, but record the exact outcome of evaluation, which
        // is needed for specialization. Propagate overflow if it occurs.
        let mut candidates = candidates
            .into_iter()
            .map(|c| match self.evaluate_candidate(stack, &c, LeakCheckHigherRankedGoal::No) {
                Ok(eval) if eval.may_apply() => {
                    Ok(Some(EvaluatedCandidate { candidate: c, evaluation: eval }))
                }
                Ok(_) => Ok(None),
                Err(OverflowError::Canonical) => Err(Overflow(OverflowError::Canonical)),
                Err(OverflowError::Error(e)) => Err(Overflow(OverflowError::Error(e))),
            })
            .flat_map(Result::transpose)
            .collect::<Result<Vec<_>, _>>()?;

        debug!(?stack, ?candidates, "winnowed to {} candidates", candidates.len());

        let has_non_region_infer = stack.obligation.predicate.has_non_region_infer();

        // If there are STILL multiple candidates, we can further
        // reduce the list by dropping duplicates -- including
        // resolving specializations.
        if candidates.len() > 1 {
            let mut i = 0;
            while i < candidates.len() {
                let should_drop_i = (0..candidates.len()).filter(|&j| i != j).any(|j| {
                    self.candidate_should_be_dropped_in_favor_of(
                        &candidates[i],
                        &candidates[j],
                        has_non_region_infer,
                    ) == DropVictim::Yes
                });
                if should_drop_i {
                    debug!(candidate = ?candidates[i], "Dropping candidate #{}/{}", i, candidates.len());
                    candidates.swap_remove(i);
                } else {
                    debug!(candidate = ?candidates[i], "Retaining candidate #{}/{}", i, candidates.len());
                    i += 1;

                    // If there are *STILL* multiple candidates, give up
                    // and report ambiguity.
                    if i > 1 {
                        debug!("multiple matches, ambig");
                        return Ok(None);
                    }
                }
            }
        }

        // If there are *NO* candidates, then there are no impls --
        // that we know of, anyway. Note that in the case where there
        // are unbound type variables within the obligation, it might
        // be the case that you could still satisfy the obligation
        // from another crate by instantiating the type variables with
        // a type from another crate that does have an impl. This case
        // is checked for in `evaluate_stack` (and hence users
        // who might care about this case, like coherence, should use
        // that function).
        if candidates.is_empty() {
            // If there's an error type, 'downgrade' our result from
            // `Err(Unimplemented)` to `Ok(None)`. This helps us avoid
            // emitting additional spurious errors, since we're guaranteed
            // to have emitted at least one.
            if stack.obligation.predicate.references_error() {
                debug!(?stack.obligation.predicate, "found error type in predicate, treating as ambiguous");
                return Ok(None);
            }
            return Err(Unimplemented);
        }

        // Just one candidate left.
        self.filter_reservation_impls(candidates.pop().unwrap().candidate)
    }

    ///////////////////////////////////////////////////////////////////////////
    // EVALUATION
    //
    // Tests whether an obligation can be selected or whether an impl
    // can be applied to particular types. It skips the "confirmation"
    // step and hence completely ignores output type parameters.
    //
    // The result is "true" if the obligation *may* hold and "false" if
    // we can be sure it does not.

    /// Evaluates whether the obligation `obligation` can be satisfied
    /// and returns an `EvaluationResult`. This is meant for the
    /// *initial* call.
    ///
    /// Do not use this directly, use `infcx.evaluate_obligation` instead.
    pub fn evaluate_root_obligation(
        &mut self,
        obligation: &PredicateObligation<'tcx>,
    ) -> Result<EvaluationResult, OverflowError> {
        debug_assert!(!self.infcx.next_trait_solver());
        self.evaluation_probe(|this, _outer_universe| {
            let goal =
                this.infcx.resolve_vars_if_possible((obligation.predicate, obligation.param_env));
            let mut result = this.evaluate_predicate_recursively(
                TraitObligationStackList::empty(&ProvisionalEvaluationCache::default()),
                obligation.clone(),
            )?;
            // If the predicate has done any inference, then downgrade the
            // result to ambiguous.
            if this.infcx.resolve_vars_if_possible(goal) != goal {
                result = result.max(EvaluatedToAmbig);
            }
            Ok(result)
        })
    }

    /// Computes the evaluation result of `op`, discarding any constraints.
    ///
    /// This also runs for leak check to allow higher ranked region errors to impact
    /// selection. By default it checks for leaks from all universes created inside of
    /// `op`, but this can be overwritten if necessary.
    fn evaluation_probe(
        &mut self,
        op: impl FnOnce(&mut Self, &mut ty::UniverseIndex) -> Result<EvaluationResult, OverflowError>,
    ) -> Result<EvaluationResult, OverflowError> {
        self.infcx.probe(|snapshot| -> Result<EvaluationResult, OverflowError> {
            let mut outer_universe = self.infcx.universe();
            let result = op(self, &mut outer_universe)?;

            match self.infcx.leak_check(outer_universe, Some(snapshot)) {
                Ok(()) => {}
                Err(_) => return Ok(EvaluatedToErr),
            }

            if self.infcx.opaque_types_added_in_snapshot(snapshot) {
                return Ok(result.max(EvaluatedToOkModuloOpaqueTypes));
            }

            if self.infcx.region_constraints_added_in_snapshot(snapshot) {
                Ok(result.max(EvaluatedToOkModuloRegions))
            } else {
                Ok(result)
            }
        })
    }

    /// Evaluates the predicates in `predicates` recursively. This may
    /// guide inference. If this is not desired, run it inside of a
    /// is run within an inference probe.
    /// `probe`.
    #[instrument(skip(self, stack), level = "debug")]
    fn evaluate_predicates_recursively<'o, I>(
        &mut self,
        stack: TraitObligationStackList<'o, 'tcx>,
        predicates: I,
    ) -> Result<EvaluationResult, OverflowError>
    where
        I: IntoIterator<Item = PredicateObligation<'tcx>> + std::fmt::Debug,
    {
        let mut result = EvaluatedToOk;
        for mut obligation in predicates {
            obligation.set_depth_from_parent(stack.depth());
            let eval = self.evaluate_predicate_recursively(stack, obligation.clone())?;
            if let EvaluatedToErr = eval {
                // fast-path - EvaluatedToErr is the top of the lattice,
                // so we don't need to look on the other predicates.
                return Ok(EvaluatedToErr);
            } else {
                result = cmp::max(result, eval);
            }
        }
        Ok(result)
    }

    #[instrument(
        level = "debug",
        skip(self, previous_stack),
        fields(previous_stack = ?previous_stack.head())
        ret,
    )]
    fn evaluate_predicate_recursively<'o>(
        &mut self,
        previous_stack: TraitObligationStackList<'o, 'tcx>,
        obligation: PredicateObligation<'tcx>,
    ) -> Result<EvaluationResult, OverflowError> {
        debug_assert!(!self.infcx.next_trait_solver());
        // `previous_stack` stores a `PolyTraitObligation`, while `obligation` is
        // a `PredicateObligation`. These are distinct types, so we can't
        // use any `Option` combinator method that would force them to be
        // the same.
        match previous_stack.head() {
            Some(h) => self.check_recursion_limit(&obligation, h.obligation)?,
            None => self.check_recursion_limit(&obligation, &obligation)?,
        }

        ensure_sufficient_stack(|| {
            let bound_predicate = obligation.predicate.kind();
            match bound_predicate.skip_binder() {
                ty::PredicateKind::Clause(ty::ClauseKind::Trait(t)) => {
                    let t = bound_predicate.rebind(t);
                    debug_assert!(!t.has_escaping_bound_vars());
                    let obligation = obligation.with(self.tcx(), t);
                    self.evaluate_trait_predicate_recursively(previous_stack, obligation)
                }

                ty::PredicateKind::Subtype(p) => {
                    let p = bound_predicate.rebind(p);
                    // Does this code ever run?
                    match self.infcx.subtype_predicate(&obligation.cause, obligation.param_env, p) {
                        Ok(Ok(InferOk { obligations, .. })) => {
                            self.evaluate_predicates_recursively(previous_stack, obligations)
                        }
                        Ok(Err(_)) => Ok(EvaluatedToErr),
                        Err(..) => Ok(EvaluatedToAmbig),
                    }
                }

                ty::PredicateKind::Coerce(p) => {
                    let p = bound_predicate.rebind(p);
                    // Does this code ever run?
                    match self.infcx.coerce_predicate(&obligation.cause, obligation.param_env, p) {
                        Ok(Ok(InferOk { obligations, .. })) => {
                            self.evaluate_predicates_recursively(previous_stack, obligations)
                        }
                        Ok(Err(_)) => Ok(EvaluatedToErr),
                        Err(..) => Ok(EvaluatedToAmbig),
                    }
                }

                ty::PredicateKind::Clause(ty::ClauseKind::WellFormed(arg)) => {
                    // So, there is a bit going on here. First, `WellFormed` predicates
                    // are coinductive, like trait predicates with auto traits.
                    // This means that we need to detect if we have recursively
                    // evaluated `WellFormed(X)`. Otherwise, we would run into
                    // a "natural" overflow error.
                    //
                    // Now, the next question is whether we need to do anything
                    // special with caching. Considering the following tree:
                    // - `WF(Foo<T>)`
                    //   - `Bar<T>: Send`
                    //     - `WF(Foo<T>)`
                    //   - `Foo<T>: Trait`
                    // In this case, the innermost `WF(Foo<T>)` should return
                    // `EvaluatedToOk`, since it's coinductive. Then if
                    // `Bar<T>: Send` is resolved to `EvaluatedToOk`, it can be
                    // inserted into a cache (because without thinking about `WF`
                    // goals, it isn't in a cycle). If `Foo<T>: Trait` later doesn't
                    // hold, then `Bar<T>: Send` shouldn't hold. Therefore, we
                    // *do* need to keep track of coinductive cycles.

                    let cache = previous_stack.cache;
                    let dfn = cache.next_dfn();

                    for stack_arg in previous_stack.cache.wf_args.borrow().iter().rev() {
                        if stack_arg.0 != arg {
                            continue;
                        }
                        debug!("WellFormed({:?}) on stack", arg);
                        if let Some(stack) = previous_stack.head {
                            // Okay, let's imagine we have two different stacks:
                            //   `T: NonAutoTrait -> WF(T) -> T: NonAutoTrait`
                            //   `WF(T) -> T: NonAutoTrait -> WF(T)`
                            // Because of this, we need to check that all
                            // predicates between the WF goals are coinductive.
                            // Otherwise, we can say that `T: NonAutoTrait` is
                            // true.
                            // Let's imagine we have a predicate stack like
                            //         `Foo: Bar -> WF(T) -> T: NonAutoTrait -> T: Auto`
                            // depth   ^1                    ^2                 ^3
                            // and the current predicate is `WF(T)`. `wf_args`
                            // would contain `(T, 1)`. We want to check all
                            // trait predicates greater than `1`. The previous
                            // stack would be `T: Auto`.
                            let cycle = stack.iter().take_while(|s| s.depth > stack_arg.1);
                            let tcx = self.tcx();
                            let cycle = cycle.map(|stack| stack.obligation.predicate.upcast(tcx));
                            if self.coinductive_match(cycle) {
                                stack.update_reached_depth(stack_arg.1);
                                return Ok(EvaluatedToOk);
                            } else {
                                return Ok(EvaluatedToAmbigStackDependent);
                            }
                        }
                        return Ok(EvaluatedToOk);
                    }

                    match wf::obligations(
                        self.infcx,
                        obligation.param_env,
                        obligation.cause.body_id,
                        obligation.recursion_depth + 1,
                        arg,
                        obligation.cause.span,
                    ) {
                        Some(obligations) => {
                            cache.wf_args.borrow_mut().push((arg, previous_stack.depth()));
                            let result =
                                self.evaluate_predicates_recursively(previous_stack, obligations);
                            cache.wf_args.borrow_mut().pop();

                            let result = result?;

                            if !result.must_apply_modulo_regions() {
                                cache.on_failure(dfn);
                            }

                            cache.on_completion(dfn);

                            Ok(result)
                        }
                        None => Ok(EvaluatedToAmbig),
                    }
                }

                ty::PredicateKind::Clause(ty::ClauseKind::TypeOutlives(pred)) => {
                    // A global type with no free lifetimes or generic parameters
                    // outlives anything.
                    if pred.0.has_free_regions()
                        || pred.0.has_bound_regions()
                        || pred.0.has_non_region_infer()
                        || pred.0.has_non_region_infer()
                    {
                        Ok(EvaluatedToOkModuloRegions)
                    } else {
                        Ok(EvaluatedToOk)
                    }
                }

                ty::PredicateKind::Clause(ty::ClauseKind::RegionOutlives(..)) => {
                    // We do not consider region relationships when evaluating trait matches.
                    Ok(EvaluatedToOkModuloRegions)
                }

                ty::PredicateKind::ObjectSafe(trait_def_id) => {
                    if self.tcx().is_object_safe(trait_def_id) {
                        Ok(EvaluatedToOk)
                    } else {
                        Ok(EvaluatedToErr)
                    }
                }

                ty::PredicateKind::Clause(ty::ClauseKind::Projection(data)) => {
                    let data = bound_predicate.rebind(data);
                    let project_obligation = obligation.with(self.tcx(), data);
                    match project::poly_project_and_unify_term(self, &project_obligation) {
                        ProjectAndUnifyResult::Holds(mut subobligations) => {
                            'compute_res: {
                                // If we've previously marked this projection as 'complete', then
                                // use the final cached result (either `EvaluatedToOk` or
                                // `EvaluatedToOkModuloRegions`), and skip re-evaluating the
                                // sub-obligations.
                                if let Some(key) =
                                    ProjectionCacheKey::from_poly_projection_obligation(
                                        self,
                                        &project_obligation,
                                    )
                                {
                                    if let Some(cached_res) = self
                                        .infcx
                                        .inner
                                        .borrow_mut()
                                        .projection_cache()
                                        .is_complete(key)
                                    {
                                        break 'compute_res Ok(cached_res);
                                    }
                                }

                                // Need to explicitly set the depth of nested goals here as
                                // projection obligations can cycle by themselves and in
                                // `evaluate_predicates_recursively` we only add the depth
                                // for parent trait goals because only these get added to the
                                // `TraitObligationStackList`.
                                for subobligation in subobligations.iter_mut() {
                                    subobligation.set_depth_from_parent(obligation.recursion_depth);
                                }
                                let res = self.evaluate_predicates_recursively(
                                    previous_stack,
                                    subobligations,
                                );
                                if let Ok(eval_rslt) = res
                                    && (eval_rslt == EvaluatedToOk
                                        || eval_rslt == EvaluatedToOkModuloRegions)
                                    && let Some(key) =
                                        ProjectionCacheKey::from_poly_projection_obligation(
                                            self,
                                            &project_obligation,
                                        )
                                {
                                    // If the result is something that we can cache, then mark this
                                    // entry as 'complete'. This will allow us to skip evaluating the
                                    // subobligations at all the next time we evaluate the projection
                                    // predicate.
                                    self.infcx
                                        .inner
                                        .borrow_mut()
                                        .projection_cache()
                                        .complete(key, eval_rslt);
                                }
                                res
                            }
                        }
                        ProjectAndUnifyResult::FailedNormalization => Ok(EvaluatedToAmbig),
                        ProjectAndUnifyResult::Recursive => Ok(EvaluatedToAmbigStackDependent),
                        ProjectAndUnifyResult::MismatchedProjectionTypes(_) => Ok(EvaluatedToErr),
                    }
                }

                ty::PredicateKind::Clause(ty::ClauseKind::ConstEvaluatable(uv)) => {
                    match const_evaluatable::is_const_evaluatable(
                        self.infcx,
                        uv,
                        obligation.param_env,
                        obligation.cause.span,
                    ) {
                        Ok(()) => Ok(EvaluatedToOk),
                        Err(NotConstEvaluatable::MentionsInfer) => Ok(EvaluatedToAmbig),
                        Err(NotConstEvaluatable::MentionsParam) => Ok(EvaluatedToErr),
                        Err(_) => Ok(EvaluatedToErr),
                    }
                }

                ty::PredicateKind::ConstEquate(c1, c2) => {
                    let tcx = self.tcx();
                    assert!(
                        tcx.features().generic_const_exprs,
                        "`ConstEquate` without a feature gate: {c1:?} {c2:?}",
                    );

                    {
                        let c1 = tcx.expand_abstract_consts(c1);
                        let c2 = tcx.expand_abstract_consts(c2);
                        debug!(
                            "evaluate_predicate_recursively: equating consts:\nc1= {:?}\nc2= {:?}",
                            c1, c2
                        );

                        use rustc_hir::def::DefKind;
                        use ty::Unevaluated;
                        match (c1.kind(), c2.kind()) {
                            (Unevaluated(a), Unevaluated(b))
                                if a.def == b.def && tcx.def_kind(a.def) == DefKind::AssocConst =>
                            {
                                if let Ok(InferOk { obligations, value: () }) = self
                                    .infcx
                                    .at(&obligation.cause, obligation.param_env)
                                    // Can define opaque types as this is only reachable with
                                    // `generic_const_exprs`
                                    .eq(
                                        DefineOpaqueTypes::Yes,
                                        ty::AliasTerm::from(a),
                                        ty::AliasTerm::from(b),
                                    )
                                {
                                    return self.evaluate_predicates_recursively(
                                        previous_stack,
                                        obligations,
                                    );
                                }
                            }
                            (_, Unevaluated(_)) | (Unevaluated(_), _) => (),
                            (_, _) => {
                                if let Ok(InferOk { obligations, value: () }) = self
                                    .infcx
                                    .at(&obligation.cause, obligation.param_env)
                                    // Can define opaque types as this is only reachable with
                                    // `generic_const_exprs`
                                    .eq(DefineOpaqueTypes::Yes, c1, c2)
                                {
                                    return self.evaluate_predicates_recursively(
                                        previous_stack,
                                        obligations,
                                    );
                                }
                            }
                        }
                    }

                    let evaluate = |c: ty::Const<'tcx>| {
                        if let ty::ConstKind::Unevaluated(unevaluated) = c.kind() {
                            match self.infcx.try_const_eval_resolve(
                                obligation.param_env,
                                unevaluated,
                                obligation.cause.span,
                            ) {
                                Ok(val) => Ok(val),
                                Err(e) => Err(e),
                            }
                        } else {
                            Ok(c)
                        }
                    };

                    match (evaluate(c1), evaluate(c2)) {
                        (Ok(c1), Ok(c2)) => {
                            match self.infcx.at(&obligation.cause, obligation.param_env).eq(
                                // Can define opaque types as this is only reachable with
                                // `generic_const_exprs`
                                DefineOpaqueTypes::Yes,
                                c1,
                                c2,
                            ) {
                                Ok(inf_ok) => self.evaluate_predicates_recursively(
                                    previous_stack,
                                    inf_ok.into_obligations(),
                                ),
                                Err(_) => Ok(EvaluatedToErr),
                            }
                        }
                        (Err(ErrorHandled::Reported(..)), _)
                        | (_, Err(ErrorHandled::Reported(..))) => Ok(EvaluatedToErr),
                        (Err(ErrorHandled::TooGeneric(..)), _)
                        | (_, Err(ErrorHandled::TooGeneric(..))) => {
                            if c1.has_non_region_infer() || c2.has_non_region_infer() {
                                Ok(EvaluatedToAmbig)
                            } else {
                                // Two different constants using generic parameters ~> error.
                                Ok(EvaluatedToErr)
                            }
                        }
                    }
                }
                ty::PredicateKind::NormalizesTo(..) => {
                    bug!("NormalizesTo is only used by the new solver")
                }
                ty::PredicateKind::AliasRelate(..) => {
                    bug!("AliasRelate is only used by the new solver")
                }
                ty::PredicateKind::Ambiguous => Ok(EvaluatedToAmbig),
                ty::PredicateKind::Clause(ty::ClauseKind::ConstArgHasType(ct, ty)) => {
                    let ct = self.infcx.shallow_resolve_const(ct);
                    let ct_ty = match ct.kind() {
                        ty::ConstKind::Infer(_) => {
                            return Ok(EvaluatedToAmbig);
                        }
                        ty::ConstKind::Error(_) => return Ok(EvaluatedToOk),
                        ty::ConstKind::Value(ty, _) => ty,
                        ty::ConstKind::Unevaluated(uv) => {
                            self.tcx().type_of(uv.def).instantiate(self.tcx(), uv.args)
                        }
                        // FIXME(generic_const_exprs): See comment in `fulfill.rs`
                        ty::ConstKind::Expr(_) => return Ok(EvaluatedToOk),
                        ty::ConstKind::Placeholder(_) => {
                            bug!("placeholder const {:?} in old solver", ct)
                        }
                        ty::ConstKind::Bound(_, _) => bug!("escaping bound vars in {:?}", ct),
                        ty::ConstKind::Param(param_ct) => {
                            param_ct.find_ty_from_env(obligation.param_env)
                        }
                    };

                    match self.infcx.at(&obligation.cause, obligation.param_env).eq(
                        // Only really excercised by generic_const_exprs
                        DefineOpaqueTypes::Yes,
                        ct_ty,
                        ty,
                    ) {
                        Ok(inf_ok) => self.evaluate_predicates_recursively(
                            previous_stack,
                            inf_ok.into_obligations(),
                        ),
                        Err(_) => Ok(EvaluatedToErr),
                    }
                }
            }
        })
    }

    #[instrument(skip(self, previous_stack), level = "debug", ret)]
    fn evaluate_trait_predicate_recursively<'o>(
        &mut self,
        previous_stack: TraitObligationStackList<'o, 'tcx>,
        mut obligation: PolyTraitObligation<'tcx>,
    ) -> Result<EvaluationResult, OverflowError> {
        if !self.is_intercrate()
            && obligation.is_global()
            && obligation.param_env.caller_bounds().iter().all(|bound| bound.has_param())
        {
            // If a param env has no global bounds, global obligations do not
            // depend on its particular value in order to work, so we can clear
            // out the param env and get better caching.
            debug!("in global");
            obligation.param_env = obligation.param_env.without_caller_bounds();
        }

        let stack = self.push_stack(previous_stack, &obligation);
        let fresh_trait_pred = stack.fresh_trait_pred;
        let param_env = obligation.param_env;

        debug!(?fresh_trait_pred);

        // If a trait predicate is in the (local or global) evaluation cache,
        // then we know it holds without cycles.
        if let Some(result) = self.check_evaluation_cache(param_env, fresh_trait_pred) {
            debug!("CACHE HIT");
            return Ok(result);
        }

        if let Some(result) = stack.cache().get_provisional(fresh_trait_pred) {
            debug!("PROVISIONAL CACHE HIT");
            stack.update_reached_depth(result.reached_depth);
            return Ok(result.result);
        }

        // Check if this is a match for something already on the
        // stack. If so, we don't want to insert the result into the
        // main cache (it is cycle dependent) nor the provisional
        // cache (which is meant for things that have completed but
        // for a "backedge" -- this result *is* the backedge).
        if let Some(cycle_result) = self.check_evaluation_cycle(&stack) {
            return Ok(cycle_result);
        }

        let (result, dep_node) = self.in_task(|this| {
            let mut result = this.evaluate_stack(&stack)?;

            // fix issue #103563, we don't normalize
            // nested obligations which produced by `TraitDef` candidate
            // (i.e. using bounds on assoc items as assumptions).
            // because we don't have enough information to
            // normalize these obligations before evaluating.
            // so we will try to normalize the obligation and evaluate again.
            // we will replace it with new solver in the future.
            if EvaluationResult::EvaluatedToErr == result
                && fresh_trait_pred.has_aliases()
                && fresh_trait_pred.is_global()
            {
                let mut nested_obligations = Vec::new();
                let predicate = normalize_with_depth_to(
                    this,
                    param_env,
                    obligation.cause.clone(),
                    obligation.recursion_depth + 1,
                    obligation.predicate,
                    &mut nested_obligations,
                );
                if predicate != obligation.predicate {
                    let mut nested_result = EvaluationResult::EvaluatedToOk;
                    for obligation in nested_obligations {
                        nested_result = cmp::max(
                            this.evaluate_predicate_recursively(previous_stack, obligation)?,
                            nested_result,
                        );
                    }

                    if nested_result.must_apply_modulo_regions() {
                        let obligation = obligation.with(this.tcx(), predicate);
                        result = cmp::max(
                            nested_result,
                            this.evaluate_trait_predicate_recursively(previous_stack, obligation)?,
                        );
                    }
                }
            }

            Ok::<_, OverflowError>(result)
        });

        let result = result?;

        if !result.must_apply_modulo_regions() {
            stack.cache().on_failure(stack.dfn);
        }

        let reached_depth = stack.reached_depth.get();
        if reached_depth >= stack.depth {
            debug!("CACHE MISS");
            self.insert_evaluation_cache(param_env, fresh_trait_pred, dep_node, result);
            stack.cache().on_completion(stack.dfn);
        } else {
            debug!("PROVISIONAL");
            debug!(
                "caching provisionally because {:?} \
                 is a cycle participant (at depth {}, reached depth {})",
                fresh_trait_pred, stack.depth, reached_depth,
            );

            stack.cache().insert_provisional(stack.dfn, reached_depth, fresh_trait_pred, result);
        }

        Ok(result)
    }

    /// If there is any previous entry on the stack that precisely
    /// matches this obligation, then we can assume that the
    /// obligation is satisfied for now (still all other conditions
    /// must be met of course). One obvious case this comes up is
    /// marker traits like `Send`. Think of a linked list:
    ///
    ///     struct List<T> { data: T, next: Option<Box<List<T>>> }
    ///
    /// `Box<List<T>>` will be `Send` if `T` is `Send` and
    /// `Option<Box<List<T>>>` is `Send`, and in turn
    /// `Option<Box<List<T>>>` is `Send` if `Box<List<T>>` is
    /// `Send`.
    ///
    /// Note that we do this comparison using the `fresh_trait_ref`
    /// fields. Because these have all been freshened using
    /// `self.freshener`, we can be sure that (a) this will not
    /// affect the inferencer state and (b) that if we see two
    /// fresh regions with the same index, they refer to the same
    /// unbound type variable.
    fn check_evaluation_cycle(
        &mut self,
        stack: &TraitObligationStack<'_, 'tcx>,
    ) -> Option<EvaluationResult> {
        if let Some(cycle_depth) = stack
            .iter()
            .skip(1) // Skip top-most frame.
            .find(|prev| {
                stack.obligation.param_env == prev.obligation.param_env
                    && stack.fresh_trait_pred == prev.fresh_trait_pred
            })
            .map(|stack| stack.depth)
        {
            debug!("evaluate_stack --> recursive at depth {}", cycle_depth);

            // If we have a stack like `A B C D E A`, where the top of
            // the stack is the final `A`, then this will iterate over
            // `A, E, D, C, B` -- i.e., all the participants apart
            // from the cycle head. We mark them as participating in a
            // cycle. This suppresses caching for those nodes. See
            // `in_cycle` field for more details.
            stack.update_reached_depth(cycle_depth);

            // Subtle: when checking for a coinductive cycle, we do
            // not compare using the "freshened trait refs" (which
            // have erased regions) but rather the fully explicit
            // trait refs. This is important because it's only a cycle
            // if the regions match exactly.
            let cycle = stack.iter().skip(1).take_while(|s| s.depth >= cycle_depth);
            let tcx = self.tcx();
            let cycle = cycle.map(|stack| stack.obligation.predicate.upcast(tcx));
            if self.coinductive_match(cycle) {
                debug!("evaluate_stack --> recursive, coinductive");
                Some(EvaluatedToOk)
            } else {
                debug!("evaluate_stack --> recursive, inductive");
                Some(EvaluatedToAmbigStackDependent)
            }
        } else {
            None
        }
    }

    fn evaluate_stack<'o>(
        &mut self,
        stack: &TraitObligationStack<'o, 'tcx>,
    ) -> Result<EvaluationResult, OverflowError> {
        debug_assert!(!self.infcx.next_trait_solver());
        // In intercrate mode, whenever any of the generics are unbound,
        // there can always be an impl. Even if there are no impls in
        // this crate, perhaps the type would be unified with
        // something from another crate that does provide an impl.
        //
        // In intra mode, we must still be conservative. The reason is
        // that we want to avoid cycles. Imagine an impl like:
        //
        //     impl<T:Eq> Eq for Vec<T>
        //
        // and a trait reference like `$0 : Eq` where `$0` is an
        // unbound variable. When we evaluate this trait-reference, we
        // will unify `$0` with `Vec<$1>` (for some fresh variable
        // `$1`), on the condition that `$1 : Eq`. We will then wind
        // up with many candidates (since that are other `Eq` impls
        // that apply) and try to winnow things down. This results in
        // a recursive evaluation that `$1 : Eq` -- as you can
        // imagine, this is just where we started. To avoid that, we
        // check for unbound variables and return an ambiguous (hence possible)
        // match if we've seen this trait before.
        //
        // This suffices to allow chains like `FnMut` implemented in
        // terms of `Fn` etc, but we could probably make this more
        // precise still.
        let unbound_input_types =
            stack.fresh_trait_pred.skip_binder().trait_ref.args.types().any(|ty| ty.is_fresh());

        if unbound_input_types
            && stack.iter().skip(1).any(|prev| {
                stack.obligation.param_env == prev.obligation.param_env
                    && self.match_fresh_trait_refs(stack.fresh_trait_pred, prev.fresh_trait_pred)
            })
        {
            debug!("evaluate_stack --> unbound argument, recursive --> giving up",);
            return Ok(EvaluatedToAmbigStackDependent);
        }

        match self.candidate_from_obligation(stack) {
            Ok(Some(c)) => self.evaluate_candidate(stack, &c, LeakCheckHigherRankedGoal::Yes),
            Ok(None) => Ok(EvaluatedToAmbig),
            Err(Overflow(OverflowError::Canonical)) => Err(OverflowError::Canonical),
            Err(..) => Ok(EvaluatedToErr),
        }
    }

    /// For defaulted traits, we use a co-inductive strategy to solve, so
    /// that recursion is ok. This routine returns `true` if the top of the
    /// stack (`cycle[0]`):
    ///
    /// - is a defaulted trait,
    /// - it also appears in the backtrace at some position `X`,
    /// - all the predicates at positions `X..` between `X` and the top are
    ///   also defaulted traits.
    pub(crate) fn coinductive_match<I>(&mut self, mut cycle: I) -> bool
    where
        I: Iterator<Item = ty::Predicate<'tcx>>,
    {
        cycle.all(|predicate| predicate.is_coinductive(self.tcx()))
    }

    /// Further evaluates `candidate` to decide whether all type parameters match and whether nested
    /// obligations are met. Returns whether `candidate` remains viable after this further
    /// scrutiny.
    ///
    /// Depending on the value of [LeakCheckHigherRankedGoal], we may ignore the binder of the goal
    /// when eagerly detecting higher ranked region errors via the `leak_check`. See that enum for
    /// more info.
    #[instrument(
        level = "debug",
        skip(self, stack),
        fields(depth = stack.obligation.recursion_depth),
        ret
    )]
    fn evaluate_candidate<'o>(
        &mut self,
        stack: &TraitObligationStack<'o, 'tcx>,
        candidate: &SelectionCandidate<'tcx>,
        leak_check_higher_ranked_goal: LeakCheckHigherRankedGoal,
    ) -> Result<EvaluationResult, OverflowError> {
        let mut result = self.evaluation_probe(|this, outer_universe| {
            // We eagerly instantiate higher ranked goals to prevent universe errors
            // from impacting candidate selection. This matches the behavior of the new
            // solver. This slightly weakens type inference.
            //
            // In case there are no unresolved type or const variables this
            // should still not be necessary to select a unique impl as any overlap
            // relying on a universe error from higher ranked goals should have resulted
            // in an overlap error in coherence.
            let p = self.infcx.enter_forall_and_leak_universe(stack.obligation.predicate);
            let obligation = stack.obligation.with(this.tcx(), ty::Binder::dummy(p));
            match leak_check_higher_ranked_goal {
                LeakCheckHigherRankedGoal::No => *outer_universe = self.infcx.universe(),
                LeakCheckHigherRankedGoal::Yes => {}
            }

            match this.confirm_candidate(&obligation, candidate.clone()) {
                Ok(selection) => {
                    debug!(?selection);
                    this.evaluate_predicates_recursively(
                        stack.list(),
                        selection.nested_obligations().into_iter(),
                    )
                }
                Err(..) => Ok(EvaluatedToErr),
            }
        })?;

        // If we erased any lifetimes, then we want to use
        // `EvaluatedToOkModuloRegions` instead of `EvaluatedToOk`
        // as your final result. The result will be cached using
        // the freshened trait predicate as a key, so we need
        // our result to be correct by *any* choice of original lifetimes,
        // not just the lifetime choice for this particular (non-erased)
        // predicate.
        // See issue #80691
        if stack.fresh_trait_pred.has_erased_regions() {
            result = result.max(EvaluatedToOkModuloRegions);
        }

        Ok(result)
    }

    fn check_evaluation_cache(
        &self,
        param_env: ty::ParamEnv<'tcx>,
        trait_pred: ty::PolyTraitPredicate<'tcx>,
    ) -> Option<EvaluationResult> {
        // Neither the global nor local cache is aware of intercrate
        // mode, so don't do any caching. In particular, we might
        // re-use the same `InferCtxt` with both an intercrate
        // and non-intercrate `SelectionContext`
        if self.is_intercrate() {
            return None;
        }

        let tcx = self.tcx();
        if self.can_use_global_caches(param_env) {
            if let Some(res) = tcx.evaluation_cache.get(&(param_env, trait_pred), tcx) {
                return Some(res);
            }
        }
        self.infcx.evaluation_cache.get(&(param_env, trait_pred), tcx)
    }

    fn insert_evaluation_cache(
        &mut self,
        param_env: ty::ParamEnv<'tcx>,
        trait_pred: ty::PolyTraitPredicate<'tcx>,
        dep_node: DepNodeIndex,
        result: EvaluationResult,
    ) {
        // Avoid caching results that depend on more than just the trait-ref
        // - the stack can create recursion.
        if result.is_stack_dependent() {
            return;
        }

        // Neither the global nor local cache is aware of intercrate
        // mode, so don't do any caching. In particular, we might
        // re-use the same `InferCtxt` with both an intercrate
        // and non-intercrate `SelectionContext`
        if self.is_intercrate() {
            return;
        }

        if self.can_use_global_caches(param_env) {
            if !trait_pred.has_infer() {
                debug!(?trait_pred, ?result, "insert_evaluation_cache global");
                // This may overwrite the cache with the same value
                // FIXME: Due to #50507 this overwrites the different values
                // This should be changed to use HashMapExt::insert_same
                // when that is fixed
                self.tcx().evaluation_cache.insert((param_env, trait_pred), dep_node, result);
                return;
            }
        }

        debug!(?trait_pred, ?result, "insert_evaluation_cache");
        self.infcx.evaluation_cache.insert((param_env, trait_pred), dep_node, result);
    }

    fn check_recursion_depth<T>(
        &self,
        depth: usize,
        error_obligation: &Obligation<'tcx, T>,
    ) -> Result<(), OverflowError>
    where
        T: Upcast<TyCtxt<'tcx>, ty::Predicate<'tcx>> + Clone,
    {
        if !self.infcx.tcx.recursion_limit().value_within_limit(depth) {
            match self.query_mode {
                TraitQueryMode::Standard => {
                    if let Some(e) = self.infcx.tainted_by_errors() {
                        return Err(OverflowError::Error(e));
                    }
                    self.infcx.err_ctxt().report_overflow_obligation(error_obligation, true);
                }
                TraitQueryMode::Canonical => {
                    return Err(OverflowError::Canonical);
                }
            }
        }
        Ok(())
    }

    /// Checks that the recursion limit has not been exceeded.
    ///
    /// The weird return type of this function allows it to be used with the `try` (`?`)
    /// operator within certain functions.
    #[inline(always)]
    fn check_recursion_limit<T: Display + TypeFoldable<TyCtxt<'tcx>>, V>(
        &self,
        obligation: &Obligation<'tcx, T>,
        error_obligation: &Obligation<'tcx, V>,
    ) -> Result<(), OverflowError>
    where
        V: Upcast<TyCtxt<'tcx>, ty::Predicate<'tcx>> + Clone,
    {
        self.check_recursion_depth(obligation.recursion_depth, error_obligation)
    }

    fn in_task<OP, R>(&mut self, op: OP) -> (R, DepNodeIndex)
    where
        OP: FnOnce(&mut Self) -> R,
    {
        let (result, dep_node) =
            self.tcx().dep_graph.with_anon_task(self.tcx(), dep_kinds::TraitSelect, || op(self));
        self.tcx().dep_graph.read_index(dep_node);
        (result, dep_node)
    }

    /// filter_impls filters candidates that have a positive impl for a negative
    /// goal and a negative impl for a positive goal
    #[instrument(level = "debug", skip(self, candidates))]
    fn filter_impls(
        &mut self,
        candidates: Vec<SelectionCandidate<'tcx>>,
        obligation: &PolyTraitObligation<'tcx>,
    ) -> Vec<SelectionCandidate<'tcx>> {
        trace!("{candidates:#?}");
        let tcx = self.tcx();
        let mut result = Vec::with_capacity(candidates.len());

        for candidate in candidates {
            if let ImplCandidate(def_id) = candidate {
                match (tcx.impl_polarity(def_id), obligation.polarity()) {
                    (ty::ImplPolarity::Reservation, _)
                    | (ty::ImplPolarity::Positive, ty::PredicatePolarity::Positive)
                    | (ty::ImplPolarity::Negative, ty::PredicatePolarity::Negative) => {
                        result.push(candidate);
                    }
                    _ => {}
                }
            } else {
                result.push(candidate);
            }
        }

        trace!("{result:#?}");
        result
    }

    /// filter_reservation_impls filter reservation impl for any goal as ambiguous
    #[instrument(level = "debug", skip(self))]
    fn filter_reservation_impls(
        &mut self,
        candidate: SelectionCandidate<'tcx>,
    ) -> SelectionResult<'tcx, SelectionCandidate<'tcx>> {
        let tcx = self.tcx();
        // Treat reservation impls as ambiguity.
        if let ImplCandidate(def_id) = candidate {
            if let ty::ImplPolarity::Reservation = tcx.impl_polarity(def_id) {
                if let Some(intercrate_ambiguity_clauses) = &mut self.intercrate_ambiguity_causes {
                    let message = tcx
                        .get_attr(def_id, sym::rustc_reservation_impl)
                        .and_then(|a| a.value_str());
                    if let Some(message) = message {
                        debug!(
                            "filter_reservation_impls: \
                                 reservation impl ambiguity on {:?}",
                            def_id
                        );
                        intercrate_ambiguity_clauses
                            .insert(IntercrateAmbiguityCause::ReservationImpl { message });
                    }
                }
                return Ok(None);
            }
        }
        Ok(Some(candidate))
    }

    fn is_knowable<'o>(&mut self, stack: &TraitObligationStack<'o, 'tcx>) -> Result<(), Conflict> {
        debug!("is_knowable(intercrate={:?})", self.is_intercrate());

        if !self.is_intercrate() {
            return Ok(());
        }

        let obligation = &stack.obligation;
        let predicate = self.infcx.resolve_vars_if_possible(obligation.predicate);

        // Okay to skip binder because of the nature of the
        // trait-ref-is-knowable check, which does not care about
        // bound regions.
        let trait_ref = predicate.skip_binder().trait_ref;

        coherence::trait_ref_is_knowable(self.infcx, trait_ref, |ty| Ok::<_, !>(ty)).into_ok()
    }

    /// Returns `true` if the global caches can be used.
    fn can_use_global_caches(&self, param_env: ty::ParamEnv<'tcx>) -> bool {
        // If there are any inference variables in the `ParamEnv`, then we
        // always use a cache local to this particular scope. Otherwise, we
        // switch to a global cache.
        if param_env.has_infer() {
            return false;
        }

        // Avoid using the master cache during coherence and just rely
        // on the local cache. This effectively disables caching
        // during coherence. It is really just a simplification to
        // avoid us having to fear that coherence results "pollute"
        // the master cache. Since coherence executes pretty quickly,
        // it's not worth going to more trouble to increase the
        // hit-rate, I don't think.
        if self.is_intercrate() {
            return false;
        }

        // Otherwise, we can use the global cache.
        true
    }

    fn check_candidate_cache(
        &mut self,
        param_env: ty::ParamEnv<'tcx>,
        cache_fresh_trait_pred: ty::PolyTraitPredicate<'tcx>,
    ) -> Option<SelectionResult<'tcx, SelectionCandidate<'tcx>>> {
        // Neither the global nor local cache is aware of intercrate
        // mode, so don't do any caching. In particular, we might
        // re-use the same `InferCtxt` with both an intercrate
        // and non-intercrate `SelectionContext`
        if self.is_intercrate() {
            return None;
        }
        let tcx = self.tcx();
        let pred = cache_fresh_trait_pred.skip_binder();

        if self.can_use_global_caches(param_env) {
            if let Some(res) = tcx.selection_cache.get(&(param_env, pred), tcx) {
                return Some(res);
            }
        }
        self.infcx.selection_cache.get(&(param_env, pred), tcx)
    }

    /// Determines whether can we safely cache the result
    /// of selecting an obligation. This is almost always `true`,
    /// except when dealing with certain `ParamCandidate`s.
    ///
    /// Ordinarily, a `ParamCandidate` will contain no inference variables,
    /// since it was usually produced directly from a `DefId`. However,
    /// certain cases (currently only librustdoc's blanket impl finder),
    /// a `ParamEnv` may be explicitly constructed with inference types.
    /// When this is the case, we do *not* want to cache the resulting selection
    /// candidate. This is due to the fact that it might not always be possible
    /// to equate the obligation's trait ref and the candidate's trait ref,
    /// if more constraints end up getting added to an inference variable.
    ///
    /// Because of this, we always want to re-run the full selection
    /// process for our obligation the next time we see it, since
    /// we might end up picking a different `SelectionCandidate` (or none at all).
    fn can_cache_candidate(
        &self,
        result: &SelectionResult<'tcx, SelectionCandidate<'tcx>>,
    ) -> bool {
        // Neither the global nor local cache is aware of intercrate
        // mode, so don't do any caching. In particular, we might
        // re-use the same `InferCtxt` with both an intercrate
        // and non-intercrate `SelectionContext`
        if self.is_intercrate() {
            return false;
        }
        match result {
            Ok(Some(SelectionCandidate::ParamCandidate(trait_ref))) => !trait_ref.has_infer(),
            _ => true,
        }
    }

    #[instrument(skip(self, param_env, cache_fresh_trait_pred, dep_node), level = "debug")]
    fn insert_candidate_cache(
        &mut self,
        param_env: ty::ParamEnv<'tcx>,
        cache_fresh_trait_pred: ty::PolyTraitPredicate<'tcx>,
        dep_node: DepNodeIndex,
        candidate: SelectionResult<'tcx, SelectionCandidate<'tcx>>,
    ) {
        let tcx = self.tcx();
        let pred = cache_fresh_trait_pred.skip_binder();

        if !self.can_cache_candidate(&candidate) {
            debug!(?pred, ?candidate, "insert_candidate_cache - candidate is not cacheable");
            return;
        }

        if self.can_use_global_caches(param_env) {
            if let Err(Overflow(OverflowError::Canonical)) = candidate {
                // Don't cache overflow globally; we only produce this in certain modes.
            } else if !pred.has_infer() {
                if !candidate.has_infer() {
                    debug!(?pred, ?candidate, "insert_candidate_cache global");
                    // This may overwrite the cache with the same value.
                    tcx.selection_cache.insert((param_env, pred), dep_node, candidate);
                    return;
                }
            }
        }

        debug!(?pred, ?candidate, "insert_candidate_cache local");
        self.infcx.selection_cache.insert((param_env, pred), dep_node, candidate);
    }

    /// Looks at the item bounds of the projection or opaque type.
    /// If this is a nested rigid projection, such as
    /// `<<T as Tr1>::Assoc as Tr2>::Assoc`, consider the item bounds
    /// on both `Tr1::Assoc` and `Tr2::Assoc`, since we may encounter
    /// relative bounds on both via the `associated_type_bounds` feature.
    pub(super) fn for_each_item_bound<T>(
        &mut self,
        mut self_ty: Ty<'tcx>,
        mut for_each: impl FnMut(&mut Self, ty::Clause<'tcx>, usize) -> ControlFlow<T, ()>,
        on_ambiguity: impl FnOnce(),
    ) -> ControlFlow<T, ()> {
        let mut idx = 0;
        let mut in_parent_alias_type = false;

        loop {
            let (kind, alias_ty) = match *self_ty.kind() {
                ty::Alias(kind @ (ty::Projection | ty::Opaque), alias_ty) => (kind, alias_ty),
                ty::Infer(ty::TyVar(_)) => {
                    on_ambiguity();
                    return ControlFlow::Continue(());
                }
                _ => return ControlFlow::Continue(()),
            };

            // HACK: On subsequent recursions, we only care about bounds that don't
            // share the same type as `self_ty`. This is because for truly rigid
            // projections, we will never be able to equate, e.g. `<T as Tr>::A`
            // with `<<T as Tr>::A as Tr>::A`.
            let relevant_bounds = if in_parent_alias_type {
                self.tcx().item_non_self_assumptions(alias_ty.def_id)
            } else {
                self.tcx().item_super_predicates(alias_ty.def_id)
            };

            for bound in relevant_bounds.instantiate(self.tcx(), alias_ty.args) {
                for_each(self, bound, idx)?;
                idx += 1;
            }

            if kind == ty::Projection {
                self_ty = alias_ty.self_ty();
            } else {
                return ControlFlow::Continue(());
            }

            in_parent_alias_type = true;
        }
    }

    /// Equates the trait in `obligation` with trait bound. If the two traits
    /// can be equated and the normalized trait bound doesn't contain inference
    /// variables or placeholders, the normalized bound is returned.
    fn match_normalize_trait_ref(
        &mut self,
        obligation: &PolyTraitObligation<'tcx>,
        placeholder_trait_ref: ty::TraitRef<'tcx>,
        trait_bound: ty::PolyTraitRef<'tcx>,
    ) -> Result<Option<ty::TraitRef<'tcx>>, ()> {
        debug_assert!(!placeholder_trait_ref.has_escaping_bound_vars());
        if placeholder_trait_ref.def_id != trait_bound.def_id() {
            // Avoid unnecessary normalization
            return Err(());
        }

        let trait_bound = self.infcx.instantiate_binder_with_fresh_vars(
            obligation.cause.span,
            HigherRankedType,
            trait_bound,
        );
        let Normalized { value: trait_bound, obligations: _ } = ensure_sufficient_stack(|| {
            normalize_with_depth(
                self,
                obligation.param_env,
                obligation.cause.clone(),
                obligation.recursion_depth + 1,
                trait_bound,
            )
        });
        self.infcx
            .at(&obligation.cause, obligation.param_env)
            .eq(DefineOpaqueTypes::No, placeholder_trait_ref, trait_bound)
            .map(|InferOk { obligations: _, value: () }| {
                // This method is called within a probe, so we can't have
                // inference variables and placeholders escape.
                if !trait_bound.has_infer() && !trait_bound.has_placeholders() {
                    Some(trait_bound)
                } else {
                    None
                }
            })
            .map_err(|_| ())
    }
    fn where_clause_may_apply<'o>(
        &mut self,
        stack: &TraitObligationStack<'o, 'tcx>,
        where_clause_trait_ref: ty::PolyTraitRef<'tcx>,
    ) -> Result<EvaluationResult, OverflowError> {
        self.evaluation_probe(|this, outer_universe| {
            // Eagerly instantiate higher ranked goals.
            //
            // See the comment in `evaluate_candidate` to see why.
            let p = self.infcx.enter_forall_and_leak_universe(stack.obligation.predicate);
            let obligation = stack.obligation.with(this.tcx(), ty::Binder::dummy(p));
            *outer_universe = self.infcx.universe();
            match this.match_where_clause_trait_ref(&obligation, where_clause_trait_ref) {
                Ok(obligations) => this.evaluate_predicates_recursively(stack.list(), obligations),
                Err(()) => Ok(EvaluatedToErr),
            }
        })
    }

    /// Return `Yes` if the obligation's predicate type applies to the env_predicate, and
    /// `No` if it does not. Return `Ambiguous` in the case that the projection type is a GAT,
    /// and applying this env_predicate constrains any of the obligation's GAT parameters.
    ///
    /// This behavior is a somewhat of a hack to prevent over-constraining inference variables
    /// in cases like #91762.
    pub(super) fn match_projection_projections(
        &mut self,
        obligation: &ProjectionTermObligation<'tcx>,
        env_predicate: PolyProjectionPredicate<'tcx>,
        potentially_unnormalized_candidates: bool,
    ) -> ProjectionMatchesProjection {
        debug_assert_eq!(obligation.predicate.def_id, env_predicate.projection_def_id());

        let mut nested_obligations = Vec::new();
        let infer_predicate = self.infcx.instantiate_binder_with_fresh_vars(
            obligation.cause.span,
            BoundRegionConversionTime::HigherRankedType,
            env_predicate,
        );
        let infer_projection = if potentially_unnormalized_candidates {
            ensure_sufficient_stack(|| {
                normalize_with_depth_to(
                    self,
                    obligation.param_env,
                    obligation.cause.clone(),
                    obligation.recursion_depth + 1,
                    infer_predicate.projection_term,
                    &mut nested_obligations,
                )
            })
        } else {
            infer_predicate.projection_term
        };

        let is_match = self
            .infcx
            .at(&obligation.cause, obligation.param_env)
            .eq(DefineOpaqueTypes::No, obligation.predicate, infer_projection)
            .is_ok_and(|InferOk { obligations, value: () }| {
                self.evaluate_predicates_recursively(
                    TraitObligationStackList::empty(&ProvisionalEvaluationCache::default()),
                    nested_obligations.into_iter().chain(obligations),
                )
                .is_ok_and(|res| res.may_apply())
            });

        if is_match {
            let generics = self.tcx().generics_of(obligation.predicate.def_id);
            // FIXME(generic-associated-types): Addresses aggressive inference in #92917.
            // If this type is a GAT, and of the GAT args resolve to something new,
            // that means that we must have newly inferred something about the GAT.
            // We should give up in that case.
            // FIXME(generic-associated-types): This only detects one layer of inference,
            // which is probably not what we actually want, but fixing it causes some ambiguity:
            // <https://github.com/rust-lang/rust/issues/125196>.
            if !generics.is_own_empty()
                && obligation.predicate.args[generics.parent_count..].iter().any(|&p| {
                    p.has_non_region_infer()
                        && match p.unpack() {
                            ty::GenericArgKind::Const(ct) => {
                                self.infcx.shallow_resolve_const(ct) != ct
                            }
                            ty::GenericArgKind::Type(ty) => self.infcx.shallow_resolve(ty) != ty,
                            ty::GenericArgKind::Lifetime(_) => false,
                        }
                })
            {
                ProjectionMatchesProjection::Ambiguous
            } else {
                ProjectionMatchesProjection::Yes
            }
        } else {
            ProjectionMatchesProjection::No
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum DropVictim {
    Yes,
    No,
}

impl DropVictim {
    fn drop_if(should_drop: bool) -> DropVictim {
        if should_drop { DropVictim::Yes } else { DropVictim::No }
    }
}

/// ## Winnowing
///
/// Winnowing is the process of attempting to resolve ambiguity by
/// probing further. During the winnowing process, we unify all
/// type variables and then we also attempt to evaluate recursive
/// bounds to see if they are satisfied.
impl<'tcx> SelectionContext<'_, 'tcx> {
    /// Returns `DropVictim::Yes` if `victim` should be dropped in favor of
    /// `other`. Generally speaking we will drop duplicate
    /// candidates and prefer where-clause candidates.
    ///
    /// See the comment for "SelectionCandidate" for more details.
    #[instrument(level = "debug", skip(self))]
    fn candidate_should_be_dropped_in_favor_of(
        &mut self,
        victim: &EvaluatedCandidate<'tcx>,
        other: &EvaluatedCandidate<'tcx>,
        has_non_region_infer: bool,
    ) -> DropVictim {
        if victim.candidate == other.candidate {
            return DropVictim::Yes;
        }

        // Check if a bound would previously have been removed when normalizing
        // the param_env so that it can be given the lowest priority. See
        // #50825 for the motivation for this.
        let is_global =
            |cand: ty::PolyTraitPredicate<'tcx>| cand.is_global() && !cand.has_bound_vars();

        // (*) Prefer `BuiltinCandidate { has_nested: false }`, `PointeeCandidate`,
        // `DiscriminantKindCandidate`, `ConstDestructCandidate`
        // to anything else.
        //
        // This is a fix for #53123 and prevents winnowing from accidentally extending the
        // lifetime of a variable.
        match (&other.candidate, &victim.candidate) {
            // FIXME(@jswrenn): this should probably be more sophisticated
            (TransmutabilityCandidate, _) | (_, TransmutabilityCandidate) => DropVictim::No,

            // (*)
            (BuiltinCandidate { has_nested: false } | ConstDestructCandidate(_), _) => {
                DropVictim::Yes
            }
            (_, BuiltinCandidate { has_nested: false } | ConstDestructCandidate(_)) => {
                DropVictim::No
            }

            (ParamCandidate(other), ParamCandidate(victim)) => {
                let same_except_bound_vars = other.skip_binder().trait_ref
                    == victim.skip_binder().trait_ref
                    && other.skip_binder().polarity == victim.skip_binder().polarity
                    && !other.skip_binder().trait_ref.has_escaping_bound_vars();
                if same_except_bound_vars {
                    // See issue #84398. In short, we can generate multiple ParamCandidates which are
                    // the same except for unused bound vars. Just pick the one with the fewest bound vars
                    // or the current one if tied (they should both evaluate to the same answer). This is
                    // probably best characterized as a "hack", since we might prefer to just do our
                    // best to *not* create essentially duplicate candidates in the first place.
                    DropVictim::drop_if(other.bound_vars().len() <= victim.bound_vars().len())
                } else {
                    DropVictim::No
                }
            }

            // Drop otherwise equivalent non-const fn pointer candidates
            (FnPointerCandidate { .. }, FnPointerCandidate { fn_host_effect }) => {
                DropVictim::drop_if(*fn_host_effect == self.tcx().consts.true_)
            }

            (
                ParamCandidate(other_cand),
                ImplCandidate(..)
                | AutoImplCandidate
                | ClosureCandidate { .. }
                | AsyncClosureCandidate
                | AsyncFnKindHelperCandidate
                | CoroutineCandidate
                | FutureCandidate
                | IteratorCandidate
                | AsyncIteratorCandidate
                | FnPointerCandidate { .. }
                | BuiltinObjectCandidate
                | BuiltinUnsizeCandidate
                | TraitUpcastingUnsizeCandidate(_)
                | BuiltinCandidate { .. }
                | TraitAliasCandidate
                | ObjectCandidate(_)
                | ProjectionCandidate(_),
            ) => {
                // We have a where clause so don't go around looking
                // for impls. Arbitrarily give param candidates priority
                // over projection and object candidates.
                //
                // Global bounds from the where clause should be ignored
                // here (see issue #50825).
                DropVictim::drop_if(!is_global(*other_cand))
            }
            (ObjectCandidate(_) | ProjectionCandidate(_), ParamCandidate(victim_cand)) => {
                // Prefer these to a global where-clause bound
                // (see issue #50825).
                if is_global(*victim_cand) { DropVictim::Yes } else { DropVictim::No }
            }
            (
                ImplCandidate(_)
                | AutoImplCandidate
                | ClosureCandidate { .. }
                | AsyncClosureCandidate
                | AsyncFnKindHelperCandidate
                | CoroutineCandidate
                | FutureCandidate
                | IteratorCandidate
                | AsyncIteratorCandidate
                | FnPointerCandidate { .. }
                | BuiltinObjectCandidate
                | BuiltinUnsizeCandidate
                | TraitUpcastingUnsizeCandidate(_)
                | BuiltinCandidate { has_nested: true }
                | TraitAliasCandidate,
                ParamCandidate(victim_cand),
            ) => {
                // Prefer these to a global where-clause bound
                // (see issue #50825).
                DropVictim::drop_if(
                    is_global(*victim_cand) && other.evaluation.must_apply_modulo_regions(),
                )
            }

            (ProjectionCandidate(i), ProjectionCandidate(j))
            | (ObjectCandidate(i), ObjectCandidate(j)) => {
                // Arbitrarily pick the lower numbered candidate for backwards
                // compatibility reasons. Don't let this affect inference.
                DropVictim::drop_if(i < j && !has_non_region_infer)
            }
            (ObjectCandidate(_), ProjectionCandidate(_))
            | (ProjectionCandidate(_), ObjectCandidate(_)) => {
                bug!("Have both object and projection candidate")
            }

            // Arbitrarily give projection and object candidates priority.
            (
                ObjectCandidate(_) | ProjectionCandidate(_),
                ImplCandidate(..)
                | AutoImplCandidate
                | ClosureCandidate { .. }
                | AsyncClosureCandidate
                | AsyncFnKindHelperCandidate
                | CoroutineCandidate
                | FutureCandidate
                | IteratorCandidate
                | AsyncIteratorCandidate
                | FnPointerCandidate { .. }
                | BuiltinObjectCandidate
                | BuiltinUnsizeCandidate
                | TraitUpcastingUnsizeCandidate(_)
                | BuiltinCandidate { .. }
                | TraitAliasCandidate,
            ) => DropVictim::Yes,

            (
                ImplCandidate(..)
                | AutoImplCandidate
                | ClosureCandidate { .. }
                | AsyncClosureCandidate
                | AsyncFnKindHelperCandidate
                | CoroutineCandidate
                | FutureCandidate
                | IteratorCandidate
                | AsyncIteratorCandidate
                | FnPointerCandidate { .. }
                | BuiltinObjectCandidate
                | BuiltinUnsizeCandidate
                | TraitUpcastingUnsizeCandidate(_)
                | BuiltinCandidate { .. }
                | TraitAliasCandidate,
                ObjectCandidate(_) | ProjectionCandidate(_),
            ) => DropVictim::No,

            (&ImplCandidate(other_def), &ImplCandidate(victim_def)) => {
                // See if we can toss out `victim` based on specialization.
                // While this requires us to know *for sure* that the `other` impl applies
                // we still use modulo regions here.
                //
                // This is fine as specialization currently assumes that specializing
                // impls have to be always applicable, meaning that the only allowed
                // region constraints may be constraints also present on the default impl.
                let tcx = self.tcx();
                if other.evaluation.must_apply_modulo_regions() {
                    if tcx.specializes((other_def, victim_def)) {
                        return DropVictim::Yes;
                    }
                }

                match tcx.impls_are_allowed_to_overlap(other_def, victim_def) {
                    // For #33140 the impl headers must be exactly equal, the trait must not have
                    // any associated items and there are no where-clauses.
                    //
                    // We can just arbitrarily drop one of the impls.
                    Some(ty::ImplOverlapKind::FutureCompatOrderDepTraitObjects) => {
                        assert_eq!(other.evaluation, victim.evaluation);
                        DropVictim::Yes
                    }
                    // For candidates which already reference errors it doesn't really
                    // matter what we do 🤷
                    Some(ty::ImplOverlapKind::Permitted { marker: false }) => {
                        DropVictim::drop_if(other.evaluation.must_apply_considering_regions())
                    }
                    Some(ty::ImplOverlapKind::Permitted { marker: true }) => {
                        // Subtle: If the predicate we are evaluating has inference
                        // variables, do *not* allow discarding candidates due to
                        // marker trait impls.
                        //
                        // Without this restriction, we could end up accidentally
                        // constraining inference variables based on an arbitrarily
                        // chosen trait impl.
                        //
                        // Imagine we have the following code:
                        //
                        // ```rust
                        // #[marker] trait MyTrait {}
                        // impl MyTrait for u8 {}
                        // impl MyTrait for bool {}
                        // ```
                        //
                        // And we are evaluating the predicate `<_#0t as MyTrait>`.
                        //
                        // During selection, we will end up with one candidate for each
                        // impl of `MyTrait`. If we were to discard one impl in favor
                        // of the other, we would be left with one candidate, causing
                        // us to "successfully" select the predicate, unifying
                        // _#0t with (for example) `u8`.
                        //
                        // However, we have no reason to believe that this unification
                        // is correct - we've essentially just picked an arbitrary
                        // *possibility* for _#0t, and required that this be the *only*
                        // possibility.
                        //
                        // Eventually, we will either:
                        // 1) Unify all inference variables in the predicate through
                        // some other means (e.g. type-checking of a function). We will
                        // then be in a position to drop marker trait candidates
                        // without constraining inference variables (since there are
                        // none left to constrain)
                        // 2) Be left with some unconstrained inference variables. We
                        // will then correctly report an inference error, since the
                        // existence of multiple marker trait impls tells us nothing
                        // about which one should actually apply.
                        DropVictim::drop_if(
                            !has_non_region_infer
                                && other.evaluation.must_apply_considering_regions(),
                        )
                    }
                    None => DropVictim::No,
                }
            }

            (AutoImplCandidate, ImplCandidate(_)) | (ImplCandidate(_), AutoImplCandidate) => {
                DropVictim::No
            }

            (AutoImplCandidate, _) | (_, AutoImplCandidate) => {
                bug!(
                    "default implementations shouldn't be recorded \
                    when there are other global candidates: {:?} {:?}",
                    other,
                    victim
                );
            }

            // Everything else is ambiguous
            (
                ImplCandidate(_)
                | ClosureCandidate { .. }
                | AsyncClosureCandidate
                | AsyncFnKindHelperCandidate
                | CoroutineCandidate
                | FutureCandidate
                | IteratorCandidate
                | AsyncIteratorCandidate
                | FnPointerCandidate { .. }
                | BuiltinObjectCandidate
                | BuiltinUnsizeCandidate
                | TraitUpcastingUnsizeCandidate(_)
                | BuiltinCandidate { has_nested: true }
                | TraitAliasCandidate,
                ImplCandidate(_)
                | ClosureCandidate { .. }
                | AsyncClosureCandidate
                | AsyncFnKindHelperCandidate
                | CoroutineCandidate
                | FutureCandidate
                | IteratorCandidate
                | AsyncIteratorCandidate
                | FnPointerCandidate { .. }
                | BuiltinObjectCandidate
                | BuiltinUnsizeCandidate
                | TraitUpcastingUnsizeCandidate(_)
                | BuiltinCandidate { has_nested: true }
                | TraitAliasCandidate,
            ) => DropVictim::No,
        }
    }
}

impl<'tcx> SelectionContext<'_, 'tcx> {
    fn sized_conditions(
        &mut self,
        obligation: &PolyTraitObligation<'tcx>,
    ) -> BuiltinImplConditions<'tcx> {
        use self::BuiltinImplConditions::{Ambiguous, None, Where};

        // NOTE: binder moved to (*)
        let self_ty = self.infcx.shallow_resolve(obligation.predicate.skip_binder().self_ty());

        match self_ty.kind() {
            ty::Infer(ty::IntVar(_) | ty::FloatVar(_))
            | ty::Uint(_)
            | ty::Int(_)
            | ty::Bool
            | ty::Float(_)
            | ty::FnDef(..)
            | ty::FnPtr(_)
            | ty::RawPtr(..)
            | ty::Char
            | ty::Ref(..)
            | ty::Coroutine(..)
            | ty::CoroutineWitness(..)
            | ty::Array(..)
            | ty::Closure(..)
            | ty::CoroutineClosure(..)
            | ty::Never
            | ty::Dynamic(_, _, ty::DynStar)
            | ty::Error(_) => {
                // safe for everything
                Where(ty::Binder::dummy(Vec::new()))
            }

            ty::Str | ty::Slice(_) | ty::Dynamic(..) | ty::Foreign(..) => None,

            ty::Tuple(tys) => Where(
                obligation.predicate.rebind(tys.last().map_or_else(Vec::new, |&last| vec![last])),
            ),

            ty::Pat(ty, _) => Where(obligation.predicate.rebind(vec![*ty])),

            ty::Adt(def, args) => {
                if let Some(sized_crit) = def.sized_constraint(self.tcx()) {
                    // (*) binder moved here
                    Where(
                        obligation.predicate.rebind(vec![sized_crit.instantiate(self.tcx(), args)]),
                    )
                } else {
                    Where(ty::Binder::dummy(Vec::new()))
                }
            }

            ty::Alias(..) | ty::Param(_) | ty::Placeholder(..) => None,
            ty::Infer(ty::TyVar(_)) => Ambiguous,

            // We can make this an ICE if/once we actually instantiate the trait obligation eagerly.
            ty::Bound(..) => None,

            ty::Infer(ty::FreshTy(_) | ty::FreshIntTy(_) | ty::FreshFloatTy(_)) => {
                bug!("asked to assemble builtin bounds of unexpected type: {:?}", self_ty);
            }
        }
    }

    fn copy_clone_conditions(
        &mut self,
        obligation: &PolyTraitObligation<'tcx>,
    ) -> BuiltinImplConditions<'tcx> {
        // NOTE: binder moved to (*)
        let self_ty = self.infcx.shallow_resolve(obligation.predicate.skip_binder().self_ty());

        use self::BuiltinImplConditions::{Ambiguous, None, Where};

        match *self_ty.kind() {
            ty::FnDef(..) | ty::FnPtr(_) | ty::Error(_) => Where(ty::Binder::dummy(Vec::new())),

            ty::Uint(_)
            | ty::Int(_)
            | ty::Infer(ty::IntVar(_) | ty::FloatVar(_))
            | ty::Bool
            | ty::Float(_)
            | ty::Char
            | ty::RawPtr(..)
            | ty::Never
            | ty::Ref(_, _, hir::Mutability::Not)
            | ty::Array(..) => {
                // Implementations provided in libcore
                None
            }

            ty::Dynamic(..)
            | ty::Str
            | ty::Slice(..)
            | ty::Foreign(..)
            | ty::Ref(_, _, hir::Mutability::Mut) => None,

            ty::Tuple(tys) => {
                // (*) binder moved here
                Where(obligation.predicate.rebind(tys.iter().collect()))
            }

            ty::Pat(ty, _) => {
                // (*) binder moved here
                Where(obligation.predicate.rebind(vec![ty]))
            }

            ty::Coroutine(coroutine_def_id, args) => {
                match self.tcx().coroutine_movability(coroutine_def_id) {
                    hir::Movability::Static => None,
                    hir::Movability::Movable => {
                        if self.tcx().features().coroutine_clone {
                            let resolved_upvars =
                                self.infcx.shallow_resolve(args.as_coroutine().tupled_upvars_ty());
                            let resolved_witness =
                                self.infcx.shallow_resolve(args.as_coroutine().witness());
                            if resolved_upvars.is_ty_var() || resolved_witness.is_ty_var() {
                                // Not yet resolved.
                                Ambiguous
                            } else {
                                let all = args
                                    .as_coroutine()
                                    .upvar_tys()
                                    .iter()
                                    .chain([args.as_coroutine().witness()])
                                    .collect::<Vec<_>>();
                                Where(obligation.predicate.rebind(all))
                            }
                        } else {
                            None
                        }
                    }
                }
            }

            ty::CoroutineWitness(def_id, args) => {
                let hidden_types = bind_coroutine_hidden_types_above(
                    self.infcx,
                    def_id,
                    args,
                    obligation.predicate.bound_vars(),
                );
                Where(hidden_types)
            }

            ty::Closure(_, args) => {
                // (*) binder moved here
                let ty = self.infcx.shallow_resolve(args.as_closure().tupled_upvars_ty());
                if let ty::Infer(ty::TyVar(_)) = ty.kind() {
                    // Not yet resolved.
                    Ambiguous
                } else {
                    Where(obligation.predicate.rebind(args.as_closure().upvar_tys().to_vec()))
                }
            }

            // FIXME(async_closures): These are never clone, for now.
            ty::CoroutineClosure(_, _) => None,
            // `Copy` and `Clone` are automatically implemented for an anonymous adt
            // if all of its fields are `Copy` and `Clone`
            ty::Adt(adt, args) if adt.is_anonymous() => {
                // (*) binder moved here
                Where(obligation.predicate.rebind(
                    adt.non_enum_variant().fields.iter().map(|f| f.ty(self.tcx(), args)).collect(),
                ))
            }

            ty::Adt(..) | ty::Alias(..) | ty::Param(..) | ty::Placeholder(..) => {
                // Fallback to whatever user-defined impls exist in this case.
                None
            }

            ty::Infer(ty::TyVar(_)) => {
                // Unbound type variable. Might or might not have
                // applicable impls and so forth, depending on what
                // those type variables wind up being bound to.
                Ambiguous
            }

            // We can make this an ICE if/once we actually instantiate the trait obligation eagerly.
            ty::Bound(..) => None,

            ty::Infer(ty::FreshTy(_) | ty::FreshIntTy(_) | ty::FreshFloatTy(_)) => {
                bug!("asked to assemble builtin bounds of unexpected type: {:?}", self_ty);
            }
        }
    }

    fn fused_iterator_conditions(
        &mut self,
        obligation: &PolyTraitObligation<'tcx>,
    ) -> BuiltinImplConditions<'tcx> {
        let self_ty = self.infcx.shallow_resolve(obligation.self_ty().skip_binder());
        if let ty::Coroutine(did, ..) = *self_ty.kind()
            && self.tcx().coroutine_is_gen(did)
        {
            BuiltinImplConditions::Where(ty::Binder::dummy(Vec::new()))
        } else {
            BuiltinImplConditions::None
        }
    }

    /// For default impls, we need to break apart a type into its
    /// "constituent types" -- meaning, the types that it contains.
    ///
    /// Here are some (simple) examples:
    ///
    /// ```ignore (illustrative)
    /// (i32, u32) -> [i32, u32]
    /// Foo where struct Foo { x: i32, y: u32 } -> [i32, u32]
    /// Bar<i32> where struct Bar<T> { x: T, y: u32 } -> [i32, u32]
    /// Zed<i32> where enum Zed { A(T), B(u32) } -> [i32, u32]
    /// ```
    #[instrument(level = "debug", skip(self), ret)]
    fn constituent_types_for_ty(
        &self,
        t: ty::Binder<'tcx, Ty<'tcx>>,
    ) -> Result<ty::Binder<'tcx, Vec<Ty<'tcx>>>, SelectionError<'tcx>> {
        Ok(match *t.skip_binder().kind() {
            ty::Uint(_)
            | ty::Int(_)
            | ty::Bool
            | ty::Float(_)
            | ty::FnDef(..)
            | ty::FnPtr(_)
            | ty::Error(_)
            | ty::Infer(ty::IntVar(_) | ty::FloatVar(_))
            | ty::Never
            | ty::Char => ty::Binder::dummy(Vec::new()),

            // Treat this like `struct str([u8]);`
            ty::Str => ty::Binder::dummy(vec![Ty::new_slice(self.tcx(), self.tcx().types.u8)]),

            ty::Placeholder(..)
            | ty::Dynamic(..)
            | ty::Param(..)
            | ty::Foreign(..)
            | ty::Alias(ty::Projection | ty::Inherent | ty::Weak, ..)
            | ty::Bound(..)
            | ty::Infer(ty::TyVar(_) | ty::FreshTy(_) | ty::FreshIntTy(_) | ty::FreshFloatTy(_)) => {
                bug!("asked to assemble constituent types of unexpected type: {:?}", t);
            }

            ty::RawPtr(element_ty, _) | ty::Ref(_, element_ty, _) => t.rebind(vec![element_ty]),

            ty::Pat(ty, _) | ty::Array(ty, _) | ty::Slice(ty) => t.rebind(vec![ty]),

            ty::Tuple(tys) => {
                // (T1, ..., Tn) -- meets any bound that all of T1...Tn meet
                t.rebind(tys.iter().collect())
            }

            ty::Closure(_, args) => {
                let ty = self.infcx.shallow_resolve(args.as_closure().tupled_upvars_ty());
                t.rebind(vec![ty])
            }

            ty::CoroutineClosure(_, args) => {
                let ty = self.infcx.shallow_resolve(args.as_coroutine_closure().tupled_upvars_ty());
                t.rebind(vec![ty])
            }

            ty::Coroutine(_, args) => {
                let ty = self.infcx.shallow_resolve(args.as_coroutine().tupled_upvars_ty());
                let witness = args.as_coroutine().witness();
                t.rebind([ty].into_iter().chain(iter::once(witness)).collect())
            }

            ty::CoroutineWitness(def_id, args) => {
                bind_coroutine_hidden_types_above(self.infcx, def_id, args, t.bound_vars())
            }

            // For `PhantomData<T>`, we pass `T`.
            ty::Adt(def, args) if def.is_phantom_data() => t.rebind(args.types().collect()),

            ty::Adt(def, args) => {
                t.rebind(def.all_fields().map(|f| f.ty(self.tcx(), args)).collect())
            }

            ty::Alias(ty::Opaque, ty::AliasTy { def_id, args, .. }) => {
                // We can resolve the `impl Trait` to its concrete type,
                // which enforces a DAG between the functions requiring
                // the auto trait bounds in question.
                match self.tcx().type_of_opaque(def_id) {
                    Ok(ty) => t.rebind(vec![ty.instantiate(self.tcx(), args)]),
                    Err(_) => {
                        return Err(SelectionError::OpaqueTypeAutoTraitLeakageUnknown(def_id));
                    }
                }
            }
        })
    }

    fn collect_predicates_for_types(
        &mut self,
        param_env: ty::ParamEnv<'tcx>,
        cause: ObligationCause<'tcx>,
        recursion_depth: usize,
        trait_def_id: DefId,
        types: ty::Binder<'tcx, Vec<Ty<'tcx>>>,
    ) -> Vec<PredicateObligation<'tcx>> {
        // Because the types were potentially derived from
        // higher-ranked obligations they may reference late-bound
        // regions. For example, `for<'a> Foo<&'a i32> : Copy` would
        // yield a type like `for<'a> &'a i32`. In general, we
        // maintain the invariant that we never manipulate bound
        // regions, so we have to process these bound regions somehow.
        //
        // The strategy is to:
        //
        // 1. Instantiate those regions to placeholder regions (e.g.,
        //    `for<'a> &'a i32` becomes `&0 i32`.
        // 2. Produce something like `&'0 i32 : Copy`
        // 3. Re-bind the regions back to `for<'a> &'a i32 : Copy`

        types
            .as_ref()
            .skip_binder() // binder moved -\
            .iter()
            .flat_map(|ty| {
                let ty: ty::Binder<'tcx, Ty<'tcx>> = types.rebind(*ty); // <----/

                let placeholder_ty = self.infcx.enter_forall_and_leak_universe(ty);
                let Normalized { value: normalized_ty, mut obligations } =
                    ensure_sufficient_stack(|| {
                        normalize_with_depth(
                            self,
                            param_env,
                            cause.clone(),
                            recursion_depth,
                            placeholder_ty,
                        )
                    });

                let tcx = self.tcx();
                let trait_ref = if tcx.generics_of(trait_def_id).own_params.len() == 1 {
                    ty::TraitRef::new(tcx, trait_def_id, [normalized_ty])
                } else {
                    // If this is an ill-formed auto/built-in trait, then synthesize
                    // new error args for the missing generics.
                    let err_args = ty::GenericArgs::extend_with_error(
                        tcx,
                        trait_def_id,
                        &[normalized_ty.into()],
                    );
                    ty::TraitRef::new_from_args(tcx, trait_def_id, err_args)
                };

                let obligation = Obligation::new(self.tcx(), cause.clone(), param_env, trait_ref);
                obligations.push(obligation);
                obligations
            })
            .collect()
    }

    ///////////////////////////////////////////////////////////////////////////
    // Matching
    //
    // Matching is a common path used for both evaluation and
    // confirmation. It basically unifies types that appear in impls
    // and traits. This does affect the surrounding environment;
    // therefore, when used during evaluation, match routines must be
    // run inside of a `probe()` so that their side-effects are
    // contained.

    fn rematch_impl(
        &mut self,
        impl_def_id: DefId,
        obligation: &PolyTraitObligation<'tcx>,
    ) -> Normalized<'tcx, GenericArgsRef<'tcx>> {
        let impl_trait_header = self.tcx().impl_trait_header(impl_def_id).unwrap();
        match self.match_impl(impl_def_id, impl_trait_header, obligation) {
            Ok(args) => args,
            Err(()) => {
                let predicate = self.infcx.resolve_vars_if_possible(obligation.predicate);
                bug!("impl {impl_def_id:?} was matchable against {predicate:?} but now is not")
            }
        }
    }

    #[instrument(level = "debug", skip(self), ret)]
    fn match_impl(
        &mut self,
        impl_def_id: DefId,
        impl_trait_header: ty::ImplTraitHeader<'tcx>,
        obligation: &PolyTraitObligation<'tcx>,
    ) -> Result<Normalized<'tcx, GenericArgsRef<'tcx>>, ()> {
        let placeholder_obligation =
            self.infcx.enter_forall_and_leak_universe(obligation.predicate);
        let placeholder_obligation_trait_ref = placeholder_obligation.trait_ref;

        let impl_args = self.infcx.fresh_args_for_item(obligation.cause.span, impl_def_id);

        let trait_ref = impl_trait_header.trait_ref.instantiate(self.tcx(), impl_args);
        if trait_ref.references_error() {
            return Err(());
        }

        debug!(?impl_trait_header);

        let Normalized { value: impl_trait_ref, obligations: mut nested_obligations } =
            ensure_sufficient_stack(|| {
                normalize_with_depth(
                    self,
                    obligation.param_env,
                    obligation.cause.clone(),
                    obligation.recursion_depth + 1,
                    trait_ref,
                )
            });

        debug!(?impl_trait_ref, ?placeholder_obligation_trait_ref);

        let cause = ObligationCause::new(
            obligation.cause.span,
            obligation.cause.body_id,
            ObligationCauseCode::MatchImpl(obligation.cause.clone(), impl_def_id),
        );

        let InferOk { obligations, .. } = self
            .infcx
            .at(&cause, obligation.param_env)
            .eq(DefineOpaqueTypes::No, placeholder_obligation_trait_ref, impl_trait_ref)
            .map_err(|e| {
                debug!("match_impl: failed eq_trait_refs due to `{}`", e.to_string(self.tcx()))
            })?;
        nested_obligations.extend(obligations);

        if !self.is_intercrate() && impl_trait_header.polarity == ty::ImplPolarity::Reservation {
            debug!("reservation impls only apply in intercrate mode");
            return Err(());
        }

        Ok(Normalized { value: impl_args, obligations: nested_obligations })
    }

    fn match_upcast_principal(
        &mut self,
        obligation: &PolyTraitObligation<'tcx>,
        unnormalized_upcast_principal: ty::PolyTraitRef<'tcx>,
        a_data: &'tcx ty::List<ty::PolyExistentialPredicate<'tcx>>,
        b_data: &'tcx ty::List<ty::PolyExistentialPredicate<'tcx>>,
        a_region: ty::Region<'tcx>,
        b_region: ty::Region<'tcx>,
    ) -> SelectionResult<'tcx, Vec<PredicateObligation<'tcx>>> {
        let tcx = self.tcx();
        let mut nested = vec![];

        // We may upcast to auto traits that are either explicitly listed in
        // the object type's bounds, or implied by the principal trait ref's
        // supertraits.
        let a_auto_traits: FxIndexSet<DefId> = a_data
            .auto_traits()
            .chain(a_data.principal_def_id().into_iter().flat_map(|principal_def_id| {
                tcx.supertrait_def_ids(principal_def_id).filter(|def_id| tcx.trait_is_auto(*def_id))
            }))
            .collect();

        let upcast_principal = normalize_with_depth_to(
            self,
            obligation.param_env,
            obligation.cause.clone(),
            obligation.recursion_depth + 1,
            unnormalized_upcast_principal,
            &mut nested,
        );

        for bound in b_data {
            match bound.skip_binder() {
                // Check that a_ty's supertrait (upcast_principal) is compatible
                // with the target (b_ty).
                ty::ExistentialPredicate::Trait(target_principal) => {
                    nested.extend(
                        self.infcx
                            .at(&obligation.cause, obligation.param_env)
                            .eq(
                                DefineOpaqueTypes::Yes,
                                upcast_principal.map_bound(|trait_ref| {
                                    ty::ExistentialTraitRef::erase_self_ty(tcx, trait_ref)
                                }),
                                bound.rebind(target_principal),
                            )
                            .map_err(|_| SelectionError::Unimplemented)?
                            .into_obligations(),
                    );
                }
                // Check that b_ty's projection is satisfied by exactly one of
                // a_ty's projections. First, we look through the list to see if
                // any match. If not, error. Then, if *more* than one matches, we
                // return ambiguity. Otherwise, if exactly one matches, equate
                // it with b_ty's projection.
                ty::ExistentialPredicate::Projection(target_projection) => {
                    let target_projection = bound.rebind(target_projection);
                    let mut matching_projections =
                        a_data.projection_bounds().filter(|source_projection| {
                            // Eager normalization means that we can just use can_eq
                            // here instead of equating and processing obligations.
                            source_projection.item_def_id() == target_projection.item_def_id()
                                && self.infcx.can_eq(
                                    obligation.param_env,
                                    *source_projection,
                                    target_projection,
                                )
                        });
                    let Some(source_projection) = matching_projections.next() else {
                        return Err(SelectionError::Unimplemented);
                    };
                    if matching_projections.next().is_some() {
                        return Ok(None);
                    }
                    nested.extend(
                        self.infcx
                            .at(&obligation.cause, obligation.param_env)
                            .eq(DefineOpaqueTypes::Yes, source_projection, target_projection)
                            .map_err(|_| SelectionError::Unimplemented)?
                            .into_obligations(),
                    );
                }
                // Check that b_ty's auto traits are present in a_ty's bounds.
                ty::ExistentialPredicate::AutoTrait(def_id) => {
                    if !a_auto_traits.contains(&def_id) {
                        return Err(SelectionError::Unimplemented);
                    }
                }
            }
        }

        nested.push(Obligation::with_depth(
            tcx,
            obligation.cause.clone(),
            obligation.recursion_depth + 1,
            obligation.param_env,
            ty::Binder::dummy(ty::OutlivesPredicate(a_region, b_region)),
        ));

        Ok(Some(nested))
    }

    /// Normalize `where_clause_trait_ref` and try to match it against
    /// `obligation`. If successful, return any predicates that
    /// result from the normalization.
    fn match_where_clause_trait_ref(
        &mut self,
        obligation: &PolyTraitObligation<'tcx>,
        where_clause_trait_ref: ty::PolyTraitRef<'tcx>,
    ) -> Result<Vec<PredicateObligation<'tcx>>, ()> {
        self.match_poly_trait_ref(obligation, where_clause_trait_ref)
    }

    /// Returns `Ok` if `poly_trait_ref` being true implies that the
    /// obligation is satisfied.
    #[instrument(skip(self), level = "debug")]
    fn match_poly_trait_ref(
        &mut self,
        obligation: &PolyTraitObligation<'tcx>,
        poly_trait_ref: ty::PolyTraitRef<'tcx>,
    ) -> Result<Vec<PredicateObligation<'tcx>>, ()> {
        let predicate = self.infcx.enter_forall_and_leak_universe(obligation.predicate);
        let trait_ref = self.infcx.instantiate_binder_with_fresh_vars(
            obligation.cause.span,
            HigherRankedType,
            poly_trait_ref,
        );
        self.infcx
            .at(&obligation.cause, obligation.param_env)
            .eq(DefineOpaqueTypes::No, predicate.trait_ref, trait_ref)
            .map(|InferOk { obligations, .. }| obligations)
            .map_err(|_| ())
    }

    ///////////////////////////////////////////////////////////////////////////
    // Miscellany

    fn match_fresh_trait_refs(
        &self,
        previous: ty::PolyTraitPredicate<'tcx>,
        current: ty::PolyTraitPredicate<'tcx>,
    ) -> bool {
        let mut matcher = _match::MatchAgainstFreshVars::new(self.tcx());
        matcher.relate(previous, current).is_ok()
    }

    fn push_stack<'o>(
        &mut self,
        previous_stack: TraitObligationStackList<'o, 'tcx>,
        obligation: &'o PolyTraitObligation<'tcx>,
    ) -> TraitObligationStack<'o, 'tcx> {
        let fresh_trait_pred = obligation.predicate.fold_with(&mut self.freshener);

        let dfn = previous_stack.cache.next_dfn();
        let depth = previous_stack.depth() + 1;
        TraitObligationStack {
            obligation,
            fresh_trait_pred,
            reached_depth: Cell::new(depth),
            previous: previous_stack,
            dfn,
            depth,
        }
    }

    #[instrument(skip(self), level = "debug")]
    fn closure_trait_ref_unnormalized(
        &mut self,
        self_ty: Ty<'tcx>,
        fn_trait_def_id: DefId,
        fn_host_effect: ty::Const<'tcx>,
    ) -> ty::PolyTraitRef<'tcx> {
        let ty::Closure(_, args) = *self_ty.kind() else {
            bug!("expected closure, found {self_ty}");
        };
        let closure_sig = args.as_closure().sig();

        closure_trait_ref_and_return_type(
            self.tcx(),
            fn_trait_def_id,
            self_ty,
            closure_sig,
            util::TupleArgumentsFlag::No,
            fn_host_effect,
        )
        .map_bound(|(trait_ref, _)| trait_ref)
    }

    /// Returns the obligations that are implied by instantiating an
    /// impl or trait. The obligations are instantiated and fully
    /// normalized. This is used when confirming an impl or default
    /// impl.
    #[instrument(level = "debug", skip(self, cause, param_env))]
    fn impl_or_trait_obligations(
        &mut self,
        cause: &ObligationCause<'tcx>,
        recursion_depth: usize,
        param_env: ty::ParamEnv<'tcx>,
        def_id: DefId,              // of impl or trait
        args: GenericArgsRef<'tcx>, // for impl or trait
        parent_trait_pred: ty::Binder<'tcx, ty::TraitPredicate<'tcx>>,
    ) -> Vec<PredicateObligation<'tcx>> {
        let tcx = self.tcx();

        // To allow for one-pass evaluation of the nested obligation,
        // each predicate must be preceded by the obligations required
        // to normalize it.
        // for example, if we have:
        //    impl<U: Iterator<Item: Copy>, V: Iterator<Item = U>> Foo for V
        // the impl will have the following predicates:
        //    <V as Iterator>::Item = U,
        //    U: Iterator, U: Sized,
        //    V: Iterator, V: Sized,
        //    <U as Iterator>::Item: Copy
        // When we instantiate, say, `V => IntoIter<u32>, U => $0`, the last
        // obligation will normalize to `<$0 as Iterator>::Item = $1` and
        // `$1: Copy`, so we must ensure the obligations are emitted in
        // that order.
        let predicates = tcx.predicates_of(def_id);
        assert_eq!(predicates.parent, None);
        let predicates = predicates.instantiate_own(tcx, args);
        let mut obligations = Vec::with_capacity(predicates.len());
        for (index, (predicate, span)) in predicates.into_iter().enumerate() {
            let cause = if tcx.is_lang_item(parent_trait_pred.def_id(), LangItem::CoerceUnsized) {
                cause.clone()
            } else {
                cause.clone().derived_cause(parent_trait_pred, |derived| {
                    ObligationCauseCode::ImplDerived(Box::new(ImplDerivedCause {
                        derived,
                        impl_or_alias_def_id: def_id,
                        impl_def_predicate_index: Some(index),
                        span,
                    }))
                })
            };
            let clause = normalize_with_depth_to(
                self,
                param_env,
                cause.clone(),
                recursion_depth,
                predicate,
                &mut obligations,
            );
            obligations.push(Obligation {
                cause,
                recursion_depth,
                param_env,
                predicate: clause.as_predicate(),
            });
        }

        obligations
    }
}

impl<'o, 'tcx> TraitObligationStack<'o, 'tcx> {
    fn list(&'o self) -> TraitObligationStackList<'o, 'tcx> {
        TraitObligationStackList::with(self)
    }

    fn cache(&self) -> &'o ProvisionalEvaluationCache<'tcx> {
        self.previous.cache
    }

    fn iter(&'o self) -> TraitObligationStackList<'o, 'tcx> {
        self.list()
    }

    /// Indicates that attempting to evaluate this stack entry
    /// required accessing something from the stack at depth `reached_depth`.
    fn update_reached_depth(&self, reached_depth: usize) {
        assert!(
            self.depth >= reached_depth,
            "invoked `update_reached_depth` with something under this stack: \
             self.depth={} reached_depth={}",
            self.depth,
            reached_depth,
        );
        debug!(reached_depth, "update_reached_depth");
        let mut p = self;
        while reached_depth < p.depth {
            debug!(?p.fresh_trait_pred, "update_reached_depth: marking as cycle participant");
            p.reached_depth.set(p.reached_depth.get().min(reached_depth));
            p = p.previous.head.unwrap();
        }
    }
}

/// The "provisional evaluation cache" is used to store intermediate cache results
/// when solving auto traits. Auto traits are unusual in that they can support
/// cycles. So, for example, a "proof tree" like this would be ok:
///
/// - `Foo<T>: Send` :-
///   - `Bar<T>: Send` :-
///     - `Foo<T>: Send` -- cycle, but ok
///   - `Baz<T>: Send`
///
/// Here, to prove `Foo<T>: Send`, we have to prove `Bar<T>: Send` and
/// `Baz<T>: Send`. Proving `Bar<T>: Send` in turn required `Foo<T>: Send`.
/// For non-auto traits, this cycle would be an error, but for auto traits (because
/// they are coinductive) it is considered ok.
///
/// However, there is a complication: at the point where we have
/// "proven" `Bar<T>: Send`, we have in fact only proven it
/// *provisionally*. In particular, we proved that `Bar<T>: Send`
/// *under the assumption* that `Foo<T>: Send`. But what if we later
/// find out this assumption is wrong?  Specifically, we could
/// encounter some kind of error proving `Baz<T>: Send`. In that case,
/// `Bar<T>: Send` didn't turn out to be true.
///
/// In Issue #60010, we found a bug in rustc where it would cache
/// these intermediate results. This was fixed in #60444 by disabling
/// *all* caching for things involved in a cycle -- in our example,
/// that would mean we don't cache that `Bar<T>: Send`. But this led
/// to large slowdowns.
///
/// Specifically, imagine this scenario, where proving `Baz<T>: Send`
/// first requires proving `Bar<T>: Send` (which is true:
///
/// - `Foo<T>: Send` :-
///   - `Bar<T>: Send` :-
///     - `Foo<T>: Send` -- cycle, but ok
///   - `Baz<T>: Send`
///     - `Bar<T>: Send` -- would be nice for this to be a cache hit!
///     - `*const T: Send` -- but what if we later encounter an error?
///
/// The *provisional evaluation cache* resolves this issue. It stores
/// cache results that we've proven but which were involved in a cycle
/// in some way. We track the minimal stack depth (i.e., the
/// farthest from the top of the stack) that we are dependent on.
/// The idea is that the cache results within are all valid -- so long as
/// none of the nodes in between the current node and the node at that minimum
/// depth result in an error (in which case the cached results are just thrown away).
///
/// During evaluation, we consult this provisional cache and rely on
/// it. Accessing a cached value is considered equivalent to accessing
/// a result at `reached_depth`, so it marks the *current* solution as
/// provisional as well. If an error is encountered, we toss out any
/// provisional results added from the subtree that encountered the
/// error. When we pop the node at `reached_depth` from the stack, we
/// can commit all the things that remain in the provisional cache.
struct ProvisionalEvaluationCache<'tcx> {
    /// next "depth first number" to issue -- just a counter
    dfn: Cell<usize>,

    /// Map from cache key to the provisionally evaluated thing.
    /// The cache entries contain the result but also the DFN in which they
    /// were added. The DFN is used to clear out values on failure.
    ///
    /// Imagine we have a stack like:
    ///
    /// - `A B C` and we add a cache for the result of C (DFN 2)
    /// - Then we have a stack `A B D` where `D` has DFN 3
    /// - We try to solve D by evaluating E: `A B D E` (DFN 4)
    /// - `E` generates various cache entries which have cyclic dependencies on `B`
    ///   - `A B D E F` and so forth
    ///   - the DFN of `F` for example would be 5
    /// - then we determine that `E` is in error -- we will then clear
    ///   all cache values whose DFN is >= 4 -- in this case, that
    ///   means the cached value for `F`.
    map: RefCell<FxIndexMap<ty::PolyTraitPredicate<'tcx>, ProvisionalEvaluation>>,

    /// The stack of args that we assume to be true because a `WF(arg)` predicate
    /// is on the stack above (and because of wellformedness is coinductive).
    /// In an "ideal" world, this would share a stack with trait predicates in
    /// `TraitObligationStack`. However, trait predicates are *much* hotter than
    /// `WellFormed` predicates, and it's very likely that the additional matches
    /// will have a perf effect. The value here is the well-formed `GenericArg`
    /// and the depth of the trait predicate *above* that well-formed predicate.
    wf_args: RefCell<Vec<(ty::GenericArg<'tcx>, usize)>>,
}

/// A cache value for the provisional cache: contains the depth-first
/// number (DFN) and result.
#[derive(Copy, Clone, Debug)]
struct ProvisionalEvaluation {
    from_dfn: usize,
    reached_depth: usize,
    result: EvaluationResult,
}

impl<'tcx> Default for ProvisionalEvaluationCache<'tcx> {
    fn default() -> Self {
        Self { dfn: Cell::new(0), map: Default::default(), wf_args: Default::default() }
    }
}

impl<'tcx> ProvisionalEvaluationCache<'tcx> {
    /// Get the next DFN in sequence (basically a counter).
    fn next_dfn(&self) -> usize {
        let result = self.dfn.get();
        self.dfn.set(result + 1);
        result
    }

    /// Check the provisional cache for any result for
    /// `fresh_trait_ref`. If there is a hit, then you must consider
    /// it an access to the stack slots at depth
    /// `reached_depth` (from the returned value).
    fn get_provisional(
        &self,
        fresh_trait_pred: ty::PolyTraitPredicate<'tcx>,
    ) -> Option<ProvisionalEvaluation> {
        debug!(
            ?fresh_trait_pred,
            "get_provisional = {:#?}",
            self.map.borrow().get(&fresh_trait_pred),
        );
        Some(*self.map.borrow().get(&fresh_trait_pred)?)
    }

    /// Insert a provisional result into the cache. The result came
    /// from the node with the given DFN. It accessed a minimum depth
    /// of `reached_depth` to compute. It evaluated `fresh_trait_pred`
    /// and resulted in `result`.
    fn insert_provisional(
        &self,
        from_dfn: usize,
        reached_depth: usize,
        fresh_trait_pred: ty::PolyTraitPredicate<'tcx>,
        result: EvaluationResult,
    ) {
        debug!(?from_dfn, ?fresh_trait_pred, ?result, "insert_provisional");

        let mut map = self.map.borrow_mut();

        // Subtle: when we complete working on the DFN `from_dfn`, anything
        // that remains in the provisional cache must be dependent on some older
        // stack entry than `from_dfn`. We have to update their depth with our transitive
        // depth in that case or else it would be referring to some popped note.
        //
        // Example:
        // A (reached depth 0)
        //   ...
        //      B // depth 1 -- reached depth = 0
        //          C // depth 2 -- reached depth = 1 (should be 0)
        //              B
        //          A // depth 0
        //   D (reached depth 1)
        //      C (cache -- reached depth = 2)
        for (_k, v) in &mut *map {
            if v.from_dfn >= from_dfn {
                v.reached_depth = reached_depth.min(v.reached_depth);
            }
        }

        map.insert(fresh_trait_pred, ProvisionalEvaluation { from_dfn, reached_depth, result });
    }

    /// Invoked when the node with dfn `dfn` does not get a successful
    /// result. This will clear out any provisional cache entries
    /// that were added since `dfn` was created. This is because the
    /// provisional entries are things which must assume that the
    /// things on the stack at the time of their creation succeeded --
    /// since the failing node is presently at the top of the stack,
    /// these provisional entries must either depend on it or some
    /// ancestor of it.
    fn on_failure(&self, dfn: usize) {
        debug!(?dfn, "on_failure");
        self.map.borrow_mut().retain(|key, eval| {
            if !eval.from_dfn >= dfn {
                debug!("on_failure: removing {:?}", key);
                false
            } else {
                true
            }
        });
    }

    /// Invoked when the node at depth `depth` completed without
    /// depending on anything higher in the stack (if that completion
    /// was a failure, then `on_failure` should have been invoked
    /// already).
    ///
    /// Note that we may still have provisional cache items remaining
    /// in the cache when this is done. For example, if there is a
    /// cycle:
    ///
    /// * A depends on...
    ///     * B depends on A
    ///     * C depends on...
    ///         * D depends on C
    ///     * ...
    ///
    /// Then as we complete the C node we will have a provisional cache
    /// with results for A, B, C, and D. This method would clear out
    /// the C and D results, but leave A and B provisional.
    ///
    /// This is determined based on the DFN: we remove any provisional
    /// results created since `dfn` started (e.g., in our example, dfn
    /// would be 2, representing the C node, and hence we would
    /// remove the result for D, which has DFN 3, but not the results for
    /// A and B, which have DFNs 0 and 1 respectively).
    ///
    /// Note that we *do not* attempt to cache these cycle participants
    /// in the evaluation cache. Doing so would require carefully computing
    /// the correct `DepNode` to store in the cache entry:
    /// cycle participants may implicitly depend on query results
    /// related to other participants in the cycle, due to our logic
    /// which examines the evaluation stack.
    ///
    /// We used to try to perform this caching,
    /// but it lead to multiple incremental compilation ICEs
    /// (see #92987 and #96319), and was very hard to understand.
    /// Fortunately, removing the caching didn't seem to
    /// have a performance impact in practice.
    fn on_completion(&self, dfn: usize) {
        debug!(?dfn, "on_completion");
        self.map.borrow_mut().retain(|fresh_trait_pred, eval| {
            if eval.from_dfn >= dfn {
                debug!(?fresh_trait_pred, ?eval, "on_completion");
                return false;
            }
            true
        });
    }
}

#[derive(Copy, Clone)]
struct TraitObligationStackList<'o, 'tcx> {
    cache: &'o ProvisionalEvaluationCache<'tcx>,
    head: Option<&'o TraitObligationStack<'o, 'tcx>>,
}

impl<'o, 'tcx> TraitObligationStackList<'o, 'tcx> {
    fn empty(cache: &'o ProvisionalEvaluationCache<'tcx>) -> TraitObligationStackList<'o, 'tcx> {
        TraitObligationStackList { cache, head: None }
    }

    fn with(r: &'o TraitObligationStack<'o, 'tcx>) -> TraitObligationStackList<'o, 'tcx> {
        TraitObligationStackList { cache: r.cache(), head: Some(r) }
    }

    fn head(&self) -> Option<&'o TraitObligationStack<'o, 'tcx>> {
        self.head
    }

    fn depth(&self) -> usize {
        if let Some(head) = self.head { head.depth } else { 0 }
    }
}

impl<'o, 'tcx> Iterator for TraitObligationStackList<'o, 'tcx> {
    type Item = &'o TraitObligationStack<'o, 'tcx>;

    fn next(&mut self) -> Option<&'o TraitObligationStack<'o, 'tcx>> {
        let o = self.head?;
        *self = o.previous;
        Some(o)
    }
}

impl<'o, 'tcx> fmt::Debug for TraitObligationStack<'o, 'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TraitObligationStack({:?})", self.obligation)
    }
}

pub enum ProjectionMatchesProjection {
    Yes,
    Ambiguous,
    No,
}

/// Replace all regions inside the coroutine interior with late bound regions.
/// Note that each region slot in the types gets a new fresh late bound region, which means that
/// none of the regions inside relate to any other, even if typeck had previously found constraints
/// that would cause them to be related.
#[instrument(level = "trace", skip(infcx), ret)]
fn bind_coroutine_hidden_types_above<'tcx>(
    infcx: &InferCtxt<'tcx>,
    def_id: DefId,
    args: ty::GenericArgsRef<'tcx>,
    bound_vars: &ty::List<ty::BoundVariableKind>,
) -> ty::Binder<'tcx, Vec<Ty<'tcx>>> {
    let tcx = infcx.tcx;
    let mut seen_tys = FxHashSet::default();

    let considering_regions = infcx.considering_regions;

    let num_bound_variables = bound_vars.len() as u32;
    let mut counter = num_bound_variables;

    let hidden_types: Vec<_> = tcx
        .coroutine_hidden_types(def_id)
        // Deduplicate tys to avoid repeated work.
        .filter(|bty| seen_tys.insert(*bty))
        .map(|mut bty| {
            // Only remap erased regions if we use them.
            if considering_regions {
                bty = bty.map_bound(|ty| {
                    tcx.fold_regions(ty, |r, current_depth| match r.kind() {
                        ty::ReErased => {
                            let br = ty::BoundRegion {
                                var: ty::BoundVar::from_u32(counter),
                                kind: ty::BrAnon,
                            };
                            counter += 1;
                            ty::Region::new_bound(tcx, current_depth, br)
                        }
                        r => bug!("unexpected region: {r:?}"),
                    })
                })
            }

            bty.instantiate(tcx, args)
        })
        .collect();
    let bound_vars =
        tcx.mk_bound_variable_kinds_from_iter(bound_vars.iter().chain(
            (num_bound_variables..counter).map(|_| ty::BoundVariableKind::Region(ty::BrAnon)),
        ));
    ty::Binder::bind_with_vars(hidden_types, bound_vars)
}
