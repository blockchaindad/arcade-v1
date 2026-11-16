//! Confirmations that count for money (swaps, payments, "confirmed" balances).
//!
//! On most chains a block on top of a payment is a confirmation: each header's work is checked
//! and its bits are tied to the chain's difficulty (MAX_EASIER or the retarget rule), so a block
//! costs what the chain's blocks cost. Where that does not hold
//! ([`ChainParams::confirms_by_proven_work`]: DigiByte, Kotia, Wojakcoin) a header at the
//! minimum difficulty, or one whose work can't be checked here (DigiByte's Groestl, Qubit and
//! Odocrypt; Kotia's staked blocks), still extends the tip, and costs next to nothing to make.
//! Sync lets such headers count only the chain's minimum work (ChainParams::header_work), so
//! they can't outweigh a real block, but they are still blocks, and a payment in one, or under
//! a run of them, proves nothing.
//!
//! So on those chains confirmations are counted in proven work: a block's counted work less the
//! minimum every block counts anyway, which is zero for a header whose work wasn't checked or
//! that sits at the minimum difficulty. The proven work from the payment's block to the tip is
//! measured against the chain's own: the average proven work per block over [`REFERENCE_SPAN`]
//! blocks ending [`REFERENCE_DEPTH`] under the tip (blocks a branch served now can only change
//! by replacing more than the undo window), taken [`PROVEN_SHARE`] times to allow for the way
//! real blocks vary (DigiByte's algorithms differ in work by orders of magnitude, and three of
//! its five carry none that is checked here). The count is never more than the plain depth. A
//! chain with no proven work in that stretch confirms nothing (Kotia after its last
//! proof-of-work height): there the answer is 0, and money paths refuse the chain.

use crate::db::{ChainDb, UNDO_DEPTH};
use arcade_chain::params::ChainParams;
use arcade_chain::pow::U256;
use arcade_store::Result;

/// How far under the tip the reference stretch ends.
pub const REFERENCE_DEPTH: u32 = UNDO_DEPTH;
/// How many blocks the reference stretch spans.
pub const REFERENCE_SPAN: u32 = 1000;
/// A block confirms with 1/PROVEN_SHARE of the reference's average proven work per block.
pub const PROVEN_SHARE: u64 = 2;

/// Counts confirmations for money on one chain, as of its tip when made ([`ChainDb::money_depth`]).
pub struct MoneyDepth<'a> {
    db: &'a ChainDb,
    params: &'a ChainParams,
    tip: Option<(u32, U256)>,
    /// None: the plain depth (a chain whose blocks' work is all checked and bounded). Some:
    /// the reference stretch's proven work and its length in blocks.
    reference: Option<(U256, u32)>,
}

impl ChainDb {
    /// Confirmations for money on `params`'s chain (see the module doc).
    pub fn money_depth<'a>(&'a self, params: &'a ChainParams) -> Result<MoneyDepth<'a>> {
        self.money_depth_with(params, REFERENCE_DEPTH, REFERENCE_SPAN)
    }

    /// [`ChainDb::money_depth`] with the reference stretch `span` blocks long, ending `depth`
    /// under the tip (all of a chain no higher than `depth`).
    pub fn money_depth_with<'a>(&'a self, params: &'a ChainParams, depth: u32, span: u32) -> Result<MoneyDepth<'a>> {
        let c = params.chain_no;
        let mut m = MoneyDepth { db: self, params, tip: None, reference: None };
        let Some(tip) = self.tip(c)? else { return Ok(m) };
        let Some(tip_work) = self.work_at(c, tip.height)? else { return Ok(m) };
        m.tip = Some((tip.height, tip_work));
        if !params.confirms_by_proven_work() {
            return Ok(m);
        }
        let top = if tip.height > depth { tip.height - depth } else { tip.height };
        let from = (top + 1).saturating_sub(span.max(1)).max(1);
        m.reference = Some((U256::ZERO, 0));
        if top >= from {
            if let (Some(low), Some(high)) = (self.work_at(c, from - 1)?, self.work_at(c, top)?) {
                m.reference = Some((proven(params, low, high, top - from + 1), top - from + 1));
            }
        }
        Ok(m)
    }

    /// Total work up to the connected block at `height`.
    fn work_at(&self, c: u16, height: u32) -> Result<Option<U256>> {
        let Some(h) = self.hash_at(c, height)? else { return Ok(None) };
        Ok(self.header(c, &h)?.map(|r| r.work))
    }
}

/// Proven work of the `n` blocks above total work `low` up to total work `high`: their counted
/// work less the minimum each counts anyway (0 if less).
fn proven(params: &ChainParams, low: U256, high: U256, n: u32) -> U256 {
    let counted = high.checked_sub(low).unwrap_or(U256::ZERO);
    let floor = params.min_work().mul_u64(n as u64).unwrap_or(U256::MAX);
    counted.checked_sub(floor).unwrap_or(U256::ZERO)
}

impl MoneyDepth<'_> {
    /// The tip's height (0 before the chain has one).
    pub fn tip(&self) -> u32 {
        self.tip.map(|t| t.0).unwrap_or(0)
    }

    /// Whether this chain confirms anything at all here: false where its reference stretch
    /// holds no proven work (every block staked, or none checked).
    pub fn confirms(&self) -> bool {
        self.tip.is_some() && self.reference.is_none_or(|(w, _)| !w.is_zero())
    }

    /// Confirmations for money of the connected block at `height` (0 above the tip).
    pub fn at(&self, height: u32) -> u32 {
        let Some((tip, tip_work)) = self.tip else { return 0 };
        if height > tip {
            return 0;
        }
        let plain = tip - height + 1;
        let Some((ref_work, ref_n)) = self.reference else { return plain };
        if ref_work.is_zero() {
            return 0;
        }
        let low = match height {
            0 => Some(U256::ZERO),
            h => self.db.work_at(self.params.chain_no, h - 1).ok().flatten(),
        };
        let Some(low) = low else { return 0 };
        let above = proven(self.params, low, tip_work, plain);
        // above / (ref_work / ref_n / PROVEN_SHARE), in whole blocks.
        let n = above.mul_u64(PROVEN_SHARE * ref_n as u64).unwrap_or(U256::MAX).div(ref_work);
        if n >= U256([plain as u64, 0, 0, 0]) {
            plain
        } else {
            n.0[0] as u32
        }
    }
}
