//! Unified Matcher Context for Percolator Markets
//!
//! Single context structure supporting multiple matcher modes:
//! - Passive (kind=0): Fixed spread around oracle price
//! - vAMM (kind=1): Spread + impact pricing with configurable liquidity curve
//!
//! All matchers use the same MatcherCtx layout with a `kind` field.

use solana_program::{
    account_info::AccountInfo,
    entrypoint::ProgramResult,
    program_error::ProgramError,
    pubkey::Pubkey,
};

use crate::{
    MatcherCall, MatcherReturn,
    CTX_VAMM_OFFSET, CTX_VAMM_LEN, MATCHER_CONTEXT_LEN,
    FLAG_VALID, FLAG_PARTIAL_OK,
};

// =============================================================================
// Matcher Kind
// =============================================================================

/// Matcher kind selector
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatcherKind {
    /// Passive: Fixed spread around oracle price
    Passive = 0,
    /// vAMM: Spread + impact pricing with configurable liquidity curve
    Vamm = 1,
}

impl TryFrom<u8> for MatcherKind {
    type Error = ProgramError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(MatcherKind::Passive),
            1 => Ok(MatcherKind::Vamm),
            _ => Err(ProgramError::InvalidInstructionData),
        }
    }
}

// =============================================================================
// Unified Matcher Context Structure
// =============================================================================

/// Magic number for initialized context ("CTAMCREP" stored, reads as "PERCMATC")
pub const MATCHER_MAGIC: u64 = 0x5045_5243_4d41_5443;
/// Current context version
pub const MATCHER_VERSION: u32 = 3;

/// Unified matcher context stored at offset 64 in matcher context account
///
/// Layout (256 bytes total):
/// ```text
/// Offset  Size  Field
/// 0       8     magic ("PERCMATC")
/// 8       4     version
/// 12      1     kind (0=Passive, 1=vAMM)
/// 13      3     _pad0
/// 16      32    lp_pda (LP PDA for signature verification)
/// 48      4     trading_fee_bps
/// 52      4     base_spread_bps
/// 56      4     max_total_bps
/// 60      4     impact_k_bps (vAMM only, 0 for passive)
/// 64      16    liquidity_notional_e6 (vAMM only, 0 for passive)
/// 80      16    max_fill_abs
/// 96      16    inventory_base
/// 112     8     last_oracle_price_e6
/// 120     8     last_exec_price_e6
/// 128     16    max_inventory_abs
/// 144     112   _reserved
/// ```
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct MatcherCtx {
    // ---- Header (16 bytes) ----
    /// Magic number for initialization check
    pub magic: u64,                     // 8 bytes, offset 0
    /// Version number
    pub version: u32,                   // 4 bytes, offset 8
    /// Matcher kind: 0 = Passive, 1 = vAMM
    pub kind: u8,                       // 1 byte, offset 12
    pub _pad0: [u8; 3],                 // 3 bytes, offset 13

    // ---- LP PDA (32 bytes) ----
    /// LP PDA that must sign matcher calls
    pub lp_pda: [u8; 32],               // 32 bytes, offset 16

    // ---- Fee/Spread Parameters (16 bytes) ----
    /// Trading fee in basis points (e.g., 5 = 0.05%)
    pub trading_fee_bps: u32,           // 4 bytes, offset 48
    /// Base spread in basis points (e.g., 10 = 0.10%)
    pub base_spread_bps: u32,           // 4 bytes, offset 52
    /// Maximum total bps cap (e.g., 200 = 2.00%)
    pub max_total_bps: u32,             // 4 bytes, offset 56
    /// Impact multiplier (vAMM only, 0 for passive)
    pub impact_k_bps: u32,              // 4 bytes, offset 60

    // ---- Liquidity/Fill Parameters (32 bytes) ----
    /// Quoting depth in notional-e6 (vAMM only, 0 for passive)
    pub liquidity_notional_e6: u128,    // 16 bytes, offset 64
    /// Maximum |exec_size| per call (0 = zero fill only)
    pub max_fill_abs: u128,             // 16 bytes, offset 80

    // ---- State (32 bytes) ----
    /// LP inventory in base units
    pub inventory_base: i128,           // 16 bytes, offset 96
    /// Last oracle price seen
    pub last_oracle_price_e6: u64,      // 8 bytes, offset 112
    /// Last execution price
    pub last_exec_price_e6: u64,        // 8 bytes, offset 120

    // ---- Limits (16 bytes) ----
    /// Maximum absolute inventory (0 = no limit)
    pub max_inventory_abs: u128,        // 16 bytes, offset 128

    // ---- Reserved (112 bytes) ----
    pub _reserved: [u8; 112],           // 112 bytes, offset 144
}

// Compile-time size check
const _: () = assert!(core::mem::size_of::<MatcherCtx>() == CTX_VAMM_LEN);

impl Default for MatcherCtx {
    fn default() -> Self {
        Self {
            magic: 0,
            version: 0,
            kind: 0,
            _pad0: [0; 3],
            lp_pda: [0; 32],
            trading_fee_bps: 0,
            base_spread_bps: 0,
            max_total_bps: 0,
            impact_k_bps: 0,
            liquidity_notional_e6: 0,
            max_fill_abs: 0,
            inventory_base: 0,
            last_oracle_price_e6: 0,
            last_exec_price_e6: 0,
            max_inventory_abs: 0,
            _reserved: [0; 112],
        }
    }
}

impl MatcherCtx {
    /// Check if context is initialized with valid magic
    pub fn is_initialized(data: &[u8]) -> bool {
        if data.len() < 8 {
            return false;
        }
        let magic = u64::from_le_bytes(data[0..8].try_into().unwrap());
        magic == MATCHER_MAGIC
    }

    /// Read context from data slice (at offset 64 in full context account)
    pub fn read_from(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() < CTX_VAMM_LEN {
            return Err(ProgramError::AccountDataTooSmall);
        }

        let magic = u64::from_le_bytes(data[0..8].try_into().unwrap());
        if magic != MATCHER_MAGIC {
            return Err(ProgramError::UninitializedAccount);
        }

        let version = u32::from_le_bytes(data[8..12].try_into().unwrap());
        let kind = data[12];

        let mut lp_pda = [0u8; 32];
        lp_pda.copy_from_slice(&data[16..48]);

        let trading_fee_bps = u32::from_le_bytes(data[48..52].try_into().unwrap());
        let base_spread_bps = u32::from_le_bytes(data[52..56].try_into().unwrap());
        let max_total_bps = u32::from_le_bytes(data[56..60].try_into().unwrap());
        let impact_k_bps = u32::from_le_bytes(data[60..64].try_into().unwrap());
        let liquidity_notional_e6 = u128::from_le_bytes(data[64..80].try_into().unwrap());
        let max_fill_abs = u128::from_le_bytes(data[80..96].try_into().unwrap());
        let inventory_base = i128::from_le_bytes(data[96..112].try_into().unwrap());
        let last_oracle_price_e6 = u64::from_le_bytes(data[112..120].try_into().unwrap());
        let last_exec_price_e6 = u64::from_le_bytes(data[120..128].try_into().unwrap());
        let max_inventory_abs = u128::from_le_bytes(data[128..144].try_into().unwrap());

        let mut reserved = [0u8; 112];
        reserved.copy_from_slice(&data[144..256]);

        Ok(Self {
            magic,
            version,
            kind,
            _pad0: [0; 3],
            lp_pda,
            trading_fee_bps,
            base_spread_bps,
            max_total_bps,
            impact_k_bps,
            liquidity_notional_e6,
            max_fill_abs,
            inventory_base,
            last_oracle_price_e6,
            last_exec_price_e6,
            max_inventory_abs,
            _reserved: reserved,
        })
    }

    /// Write context to data slice (at offset 64 in full context account)
    pub fn write_to(&self, data: &mut [u8]) -> Result<(), ProgramError> {
        if data.len() < CTX_VAMM_LEN {
            return Err(ProgramError::AccountDataTooSmall);
        }

        data[0..8].copy_from_slice(&self.magic.to_le_bytes());
        data[8..12].copy_from_slice(&self.version.to_le_bytes());
        data[12] = self.kind;
        data[13..16].copy_from_slice(&self._pad0);
        data[16..48].copy_from_slice(&self.lp_pda);
        data[48..52].copy_from_slice(&self.trading_fee_bps.to_le_bytes());
        data[52..56].copy_from_slice(&self.base_spread_bps.to_le_bytes());
        data[56..60].copy_from_slice(&self.max_total_bps.to_le_bytes());
        data[60..64].copy_from_slice(&self.impact_k_bps.to_le_bytes());
        data[64..80].copy_from_slice(&self.liquidity_notional_e6.to_le_bytes());
        data[80..96].copy_from_slice(&self.max_fill_abs.to_le_bytes());
        data[96..112].copy_from_slice(&self.inventory_base.to_le_bytes());
        data[112..120].copy_from_slice(&self.last_oracle_price_e6.to_le_bytes());
        data[120..128].copy_from_slice(&self.last_exec_price_e6.to_le_bytes());
        data[128..144].copy_from_slice(&self.max_inventory_abs.to_le_bytes());
        data[144..256].copy_from_slice(&self._reserved);

        Ok(())
    }

    /// Get the matcher kind
    pub fn get_kind(&self) -> Result<MatcherKind, ProgramError> {
        MatcherKind::try_from(self.kind)
    }

    /// Get LP PDA as Pubkey
    pub fn get_lp_pda(&self) -> Pubkey {
        Pubkey::new_from_array(self.lp_pda)
    }

    /// Validate context parameters
    pub fn validate(&self) -> Result<(), ProgramError> {
        let kind = self.get_kind()?;

        // For vAMM, liquidity_notional_e6 MUST be non-zero
        if kind == MatcherKind::Vamm && self.liquidity_notional_e6 == 0 {
            return Err(ProgramError::InvalidAccountData);
        }

        // max_total_bps MUST be <= 9000
        if self.max_total_bps > 9000 {
            return Err(ProgramError::InvalidAccountData);
        }

        // trading_fee_bps should be reasonable (< 1000 = 10%)
        if self.trading_fee_bps > 1000 {
            return Err(ProgramError::InvalidAccountData);
        }

        // base_spread_bps + trading_fee_bps should not exceed max_total_bps
        let total_fixed = self.base_spread_bps.saturating_add(self.trading_fee_bps);
        if total_fixed > self.max_total_bps {
            return Err(ProgramError::InvalidAccountData);
        }

        // LP PDA must be non-zero
        if self.lp_pda == [0u8; 32] {
            return Err(ProgramError::InvalidAccountData);
        }

        Ok(())
    }
}

// =============================================================================
// Init Instruction Layout (Tag 2)
// =============================================================================
// Offset  Field                   Type    Size
// 0       tag                     u8      1     Always 2
// 1       kind                    u8      1     0=Passive, 1=vAMM
// 2-6     trading_fee_bps         u32     4
// 6-10    base_spread_bps         u32     4
// 10-14   max_total_bps           u32     4
// 14-18   impact_k_bps            u32     4     (vAMM only)
// 18-34   liquidity_notional_e6   u128    16    (vAMM only, 0 for Passive)
// 34-50   max_fill_abs            u128    16
// 50-66   max_inventory_abs       u128    16    (0 = no limit)
// Total: 66 bytes

pub const INIT_CTX_LEN: usize = 66;

/// Parsed Init instruction parameters
#[derive(Clone, Copy, Debug)]
pub struct InitParams {
    pub kind: u8,
    pub trading_fee_bps: u32,
    pub base_spread_bps: u32,
    pub max_total_bps: u32,
    pub impact_k_bps: u32,
    pub liquidity_notional_e6: u128,
    pub max_fill_abs: u128,
    pub max_inventory_abs: u128,
}

impl InitParams {
    pub fn parse(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() < INIT_CTX_LEN {
            return Err(ProgramError::InvalidInstructionData);
        }
        if data[0] != crate::MATCHER_INIT_VAMM_TAG {
            return Err(ProgramError::InvalidInstructionData);
        }

        Ok(Self {
            kind: data[1],
            trading_fee_bps: u32::from_le_bytes(data[2..6].try_into().unwrap()),
            base_spread_bps: u32::from_le_bytes(data[6..10].try_into().unwrap()),
            max_total_bps: u32::from_le_bytes(data[10..14].try_into().unwrap()),
            impact_k_bps: u32::from_le_bytes(data[14..18].try_into().unwrap()),
            liquidity_notional_e6: u128::from_le_bytes(data[18..34].try_into().unwrap()),
            max_fill_abs: u128::from_le_bytes(data[34..50].try_into().unwrap()),
            max_inventory_abs: u128::from_le_bytes(data[50..66].try_into().unwrap()),
        })
    }

    /// Encode Init instruction data
    pub fn encode(&self) -> [u8; INIT_CTX_LEN] {
        let mut data = [0u8; INIT_CTX_LEN];
        data[0] = crate::MATCHER_INIT_VAMM_TAG;
        data[1] = self.kind;
        data[2..6].copy_from_slice(&self.trading_fee_bps.to_le_bytes());
        data[6..10].copy_from_slice(&self.base_spread_bps.to_le_bytes());
        data[10..14].copy_from_slice(&self.max_total_bps.to_le_bytes());
        data[14..18].copy_from_slice(&self.impact_k_bps.to_le_bytes());
        data[18..34].copy_from_slice(&self.liquidity_notional_e6.to_le_bytes());
        data[34..50].copy_from_slice(&self.max_fill_abs.to_le_bytes());
        data[50..66].copy_from_slice(&self.max_inventory_abs.to_le_bytes());
        data
    }
}

// =============================================================================
// Instruction Processing
// =============================================================================

/// Process Init instruction (Tag 2)
///
/// Initializes the matcher context with the specified parameters.
/// Can only be called once (context must be uninitialized).
///
/// Accounts:
/// 0. `[]` LP PDA (stored for signature verification)
/// 1. `[writable]` Matcher context account (owned by this program)
pub fn process_init(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    use solana_program::account_info::next_account_info;

    let account_iter = &mut accounts.iter();
    let lp_pda = next_account_info(account_iter)?;
    let ctx_account = next_account_info(account_iter)?;

    // Verify ownership
    if ctx_account.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }

    // Verify size
    if ctx_account.data_len() < MATCHER_CONTEXT_LEN {
        return Err(ProgramError::AccountDataTooSmall);
    }

    // Verify writable
    if !ctx_account.is_writable {
        return Err(ProgramError::InvalidAccountData);
    }

    // Parse parameters
    let params = InitParams::parse(instruction_data)?;

    // Validate kind
    let _ = MatcherKind::try_from(params.kind)?;

    // Check not already initialized
    {
        let data = ctx_account.try_borrow_data()?;
        if MatcherCtx::is_initialized(&data[CTX_VAMM_OFFSET..]) {
            return Err(ProgramError::AccountAlreadyInitialized);
        }
    }

    // Create and validate context
    let ctx = MatcherCtx {
        magic: MATCHER_MAGIC,
        version: MATCHER_VERSION,
        kind: params.kind,
        _pad0: [0; 3],
        lp_pda: lp_pda.key.to_bytes(),
        trading_fee_bps: params.trading_fee_bps,
        base_spread_bps: params.base_spread_bps,
        max_total_bps: params.max_total_bps,
        impact_k_bps: params.impact_k_bps,
        liquidity_notional_e6: params.liquidity_notional_e6,
        max_fill_abs: params.max_fill_abs,
        inventory_base: 0,
        last_oracle_price_e6: 0,
        last_exec_price_e6: 0,
        max_inventory_abs: params.max_inventory_abs,
        _reserved: [0; 112],
    };

    ctx.validate()?;

    // Write context
    let mut data = ctx_account.try_borrow_mut_data()?;
    ctx.write_to(&mut data[CTX_VAMM_OFFSET..])?;

    Ok(())
}

/// Process matcher call
///
/// Computes execution price using the configured kind (Passive or vAMM),
/// validates LP PDA signature, updates inventory, and writes result.
///
/// Accounts:
/// 0. `[signer]` LP PDA (must match stored PDA)
/// 1. `[writable]` Matcher context account
pub fn process_call(
    lp_pda: &AccountInfo,
    ctx_account: &AccountInfo,
    instruction_data: &[u8],
) -> ProgramResult {
    // Parse call
    let call = MatcherCall::parse(instruction_data)?;

    // Validate inputs
    if call.oracle_price_e6 == 0 {
        return Err(ProgramError::InvalidInstructionData);
    }
    if call.req_size == i128::MIN {
        return Err(ProgramError::InvalidInstructionData);
    }

    // Read context
    let mut ctx = {
        let data = ctx_account.try_borrow_data()?;
        MatcherCtx::read_from(&data[CTX_VAMM_OFFSET..])?
    };
    ctx.validate()?;

    // Validate LP PDA matches stored PDA
    if lp_pda.key.to_bytes() != ctx.lp_pda {
        return Err(ProgramError::InvalidAccountData);
    }

    // Compute execution based on kind
    let (exec_price, exec_size, flags) = compute_execution(&ctx, &call)?;

    // Update context state if fill occurred
    if exec_size != 0 {
        ctx.inventory_base = ctx.inventory_base.saturating_sub(exec_size);
        ctx.last_oracle_price_e6 = call.oracle_price_e6;
        ctx.last_exec_price_e6 = exec_price;
    }

    // Write updated context
    {
        let mut data = ctx_account.try_borrow_mut_data()?;
        ctx.write_to(&mut data[CTX_VAMM_OFFSET..])?;
    }

    // Write return
    let ret = MatcherReturn {
        abi_version: crate::MATCHER_ABI_VERSION,
        flags,
        exec_price_e6: exec_price,
        exec_size,
        req_id: call.req_id,
        lp_account_id: call.lp_account_id,
        oracle_price_e6: call.oracle_price_e6,
        reserved: 0,
    };

    let mut data = ctx_account.try_borrow_mut_data()?;
    ret.write_to(&mut data)?;

    Ok(())
}

/// Compute execution price and size based on matcher kind
fn compute_execution(
    ctx: &MatcherCtx,
    call: &MatcherCall,
) -> Result<(u64, i128, u32), ProgramError> {
    let kind = ctx.get_kind()?;

    match kind {
        MatcherKind::Passive => compute_passive_execution(ctx, call),
        MatcherKind::Vamm => compute_vamm_execution(ctx, call),
    }
}

/// Compute passive execution (fixed spread around oracle)
fn compute_passive_execution(
    ctx: &MatcherCtx,
    call: &MatcherCall,
) -> Result<(u64, i128, u32), ProgramError> {
    let req_abs = call.req_size.unsigned_abs();
    let is_buy = call.req_size > 0;

    // Determine fill size (cap by max_fill_abs)
    let fill_abs = if ctx.max_fill_abs == 0 {
        0u128
    } else {
        core::cmp::min(req_abs, ctx.max_fill_abs)
    };

    // Check inventory limit
    let fill_abs = check_inventory_limit(ctx, fill_abs, is_buy)?;

    // Zero fill case
    if fill_abs == 0 {
        return Ok((call.oracle_price_e6, 0, FLAG_VALID | FLAG_PARTIAL_OK));
    }

    let exec_size = if is_buy {
        fill_abs as i128
    } else {
        -(fill_abs as i128)
    };

    // Passive: total_bps = base_spread_bps + trading_fee_bps
    let base = ctx.base_spread_bps as u128;
    let fee = ctx.trading_fee_bps as u128;
    let max_total = ctx.max_total_bps as u128;
    let total_bps = core::cmp::min(max_total, base + fee);

    const BPS_DENOM: u128 = 10_000;
    let oracle = call.oracle_price_e6 as u128;

    let exec_price_u128 = if is_buy {
        oracle
            .checked_mul(BPS_DENOM + total_bps)
            .ok_or(ProgramError::ArithmeticOverflow)?
            / BPS_DENOM
    } else {
        oracle
            .checked_mul(BPS_DENOM - total_bps)
            .ok_or(ProgramError::ArithmeticOverflow)?
            / BPS_DENOM
    };

    if exec_price_u128 == 0 || exec_price_u128 > u64::MAX as u128 {
        return Err(ProgramError::ArithmeticOverflow);
    }

    Ok((exec_price_u128 as u64, exec_size, FLAG_VALID))
}

/// Compute vAMM execution (spread + impact based on liquidity curve)
fn compute_vamm_execution(
    ctx: &MatcherCtx,
    call: &MatcherCall,
) -> Result<(u64, i128, u32), ProgramError> {
    let req_abs = call.req_size.unsigned_abs();
    let is_buy = call.req_size > 0;

    // Determine fill size (cap by max_fill_abs)
    let fill_abs = if ctx.max_fill_abs == 0 {
        0u128
    } else {
        core::cmp::min(req_abs, ctx.max_fill_abs)
    };

    // Check inventory limit
    let fill_abs = check_inventory_limit(ctx, fill_abs, is_buy)?;

    // Zero fill case
    if fill_abs == 0 {
        return Ok((call.oracle_price_e6, 0, FLAG_VALID | FLAG_PARTIAL_OK));
    }

    let exec_size = if is_buy {
        fill_abs as i128
    } else {
        -(fill_abs as i128)
    };

    // Compute notional for the fill
    let oracle = call.oracle_price_e6 as u128;
    let abs_notional_e6 = fill_abs
        .checked_mul(oracle)
        .ok_or(ProgramError::ArithmeticOverflow)?
        / 1_000_000u128;

    // Compute impact in bps
    let impact_k = ctx.impact_k_bps as u128;
    let impact_bps = if ctx.liquidity_notional_e6 > 0 {
        abs_notional_e6
            .checked_mul(impact_k)
            .ok_or(ProgramError::ArithmeticOverflow)?
            / ctx.liquidity_notional_e6
    } else {
        0
    };

    // Total = base_spread + trading_fee + impact, capped at max_total
    let base = ctx.base_spread_bps as u128;
    let fee = ctx.trading_fee_bps as u128;
    let max_total = ctx.max_total_bps as u128;
    let max_impact = max_total.saturating_sub(base).saturating_sub(fee);
    let clamped_impact = core::cmp::min(impact_bps, max_impact);

    let total_bps = core::cmp::min(max_total, base + fee + clamped_impact);

    const BPS_DENOM: u128 = 10_000;

    let exec_price_u128 = if is_buy {
        oracle
            .checked_mul(BPS_DENOM + total_bps)
            .ok_or(ProgramError::ArithmeticOverflow)?
            / BPS_DENOM
    } else {
        oracle
            .checked_mul(BPS_DENOM - total_bps)
            .ok_or(ProgramError::ArithmeticOverflow)?
            / BPS_DENOM
    };

    if exec_price_u128 == 0 || exec_price_u128 > u64::MAX as u128 {
        return Err(ProgramError::ArithmeticOverflow);
    }

    Ok((exec_price_u128 as u64, exec_size, FLAG_VALID))
}

/// Check and enforce inventory limit
fn check_inventory_limit(ctx: &MatcherCtx, fill_abs: u128, is_buy: bool) -> Result<u128, ProgramError> {
    if ctx.max_inventory_abs == 0 {
        return Ok(fill_abs);
    }

    let current_inv = ctx.inventory_base;
    let max_inv = ctx.max_inventory_abs as i128;

    let inv_delta = if is_buy {
        -(fill_abs as i128)
    } else {
        fill_abs as i128
    };

    let new_inv = current_inv.saturating_add(inv_delta);

    if new_inv.unsigned_abs() <= ctx.max_inventory_abs {
        return Ok(fill_abs);
    }

    if is_buy {
        if current_inv <= -max_inv {
            return Ok(0);
        }
        let max_fill = (current_inv + max_inv).unsigned_abs();
        Ok(core::cmp::min(fill_abs, max_fill))
    } else {
        if current_inv >= max_inv {
            return Ok(0);
        }
        let max_fill = (max_inv - current_inv).unsigned_abs();
        Ok(core::cmp::min(fill_abs, max_fill))
    }
}

// =============================================================================
// Legacy exports for backward compatibility
// =============================================================================

// Re-export old names for lib.rs compatibility
pub use MatcherCtx as VammCtx;
pub use MatcherKind as MatcherMode;
pub use MATCHER_MAGIC as VAMM_MAGIC;
pub use process_init as process_init_vamm;
pub use process_call as process_vamm_call;

// Legacy type aliases
pub type InitVammParams = InitParams;
pub const INIT_VAMM_LEN: usize = INIT_CTX_LEN;

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn default_vamm_ctx() -> MatcherCtx {
        MatcherCtx {
            magic: MATCHER_MAGIC,
            version: MATCHER_VERSION,
            kind: MatcherKind::Vamm as u8,
            _pad0: [0; 3],
            lp_pda: [1; 32],
            trading_fee_bps: 5,
            base_spread_bps: 10,
            max_total_bps: 200,
            impact_k_bps: 100,
            liquidity_notional_e6: 1_000_000_000_000,
            max_fill_abs: 1_000_000_000,
            inventory_base: 0,
            last_oracle_price_e6: 0,
            last_exec_price_e6: 0,
            max_inventory_abs: 0,
            _reserved: [0; 112],
        }
    }

    fn default_passive_ctx() -> MatcherCtx {
        MatcherCtx {
            magic: MATCHER_MAGIC,
            version: MATCHER_VERSION,
            kind: MatcherKind::Passive as u8,
            _pad0: [0; 3],
            lp_pda: [1; 32],
            trading_fee_bps: 5,
            base_spread_bps: 50,
            max_total_bps: 200,
            impact_k_bps: 0,
            liquidity_notional_e6: 0,
            max_fill_abs: 1_000_000_000,
            inventory_base: 0,
            last_oracle_price_e6: 0,
            last_exec_price_e6: 0,
            max_inventory_abs: 0,
            _reserved: [0; 112],
        }
    }

    fn make_call(oracle_price: u64, req_size: i128) -> MatcherCall {
        MatcherCall {
            req_id: 1,
            lp_idx: 0,
            lp_account_id: 100,
            oracle_price_e6: oracle_price,
            req_size,
        }
    }

    #[test]
    fn test_vamm_buy_adds_spread_and_fee() {
        let ctx = default_vamm_ctx();
        let call = make_call(100_000_000, 1000);

        let (exec_price, exec_size, flags) = compute_execution(&ctx, &call).unwrap();

        assert!(exec_price >= call.oracle_price_e6);
        assert_eq!(exec_size, 1000);
        assert_eq!(flags, FLAG_VALID);
        assert!(exec_price >= 100_015_000);
    }

    #[test]
    fn test_passive_buy_adds_spread_and_fee() {
        let ctx = default_passive_ctx();
        let call = make_call(100_000_000, 1000);

        let (exec_price, exec_size, flags) = compute_execution(&ctx, &call).unwrap();

        assert!(exec_price >= call.oracle_price_e6);
        assert_eq!(exec_size, 1000);
        assert_eq!(flags, FLAG_VALID);
        assert_eq!(exec_price, 100_550_000);
    }

    #[test]
    fn test_vamm_sell_subtracts_spread() {
        let ctx = default_vamm_ctx();
        let call = make_call(100_000_000, -1000);

        let (exec_price, exec_size, flags) = compute_execution(&ctx, &call).unwrap();

        assert!(exec_price <= call.oracle_price_e6);
        assert_eq!(exec_size, -1000);
        assert_eq!(flags, FLAG_VALID);
    }

    #[test]
    fn test_vamm_bigger_size_more_impact() {
        let ctx = default_vamm_ctx();

        let call_small = make_call(100_000_000, 1_000);
        let (price_small, _, _) = compute_execution(&ctx, &call_small).unwrap();

        let call_large = make_call(100_000_000, 100_000_000);
        let (price_large, _, _) = compute_execution(&ctx, &call_large).unwrap();

        assert!(price_large > price_small);
    }

    #[test]
    fn test_total_capped_at_max() {
        let ctx = default_vamm_ctx();

        let call = make_call(100_000_000, 1_000_000_000);
        let (exec_price, _, _) = compute_execution(&ctx, &call).unwrap();

        let max_price = 100_000_000u64 * 10_200 / 10_000;
        assert!(exec_price <= max_price);
    }

    #[test]
    fn test_zero_fill_when_max_fill_zero() {
        let mut ctx = default_vamm_ctx();
        ctx.max_fill_abs = 0;

        let call = make_call(100_000_000, 1000);
        let (exec_price, exec_size, flags) = compute_execution(&ctx, &call).unwrap();

        assert_eq!(exec_size, 0);
        assert_eq!(flags, FLAG_VALID | FLAG_PARTIAL_OK);
        assert_eq!(exec_price, call.oracle_price_e6);
    }

    #[test]
    fn test_partial_fill_capped() {
        let mut ctx = default_vamm_ctx();
        ctx.max_fill_abs = 500;

        let call = make_call(100_000_000, 1000);
        let (_, exec_size, _) = compute_execution(&ctx, &call).unwrap();

        assert_eq!(exec_size, 500);
    }

    #[test]
    fn test_inventory_limit_caps_fill() {
        let mut ctx = default_vamm_ctx();
        ctx.max_inventory_abs = 100;
        ctx.inventory_base = 0;

        let call = make_call(100_000_000, 1000);
        let (_, exec_size, _) = compute_execution(&ctx, &call).unwrap();

        assert_eq!(exec_size, 100);
    }

    #[test]
    fn test_inventory_limit_at_boundary() {
        let mut ctx = default_vamm_ctx();
        ctx.max_inventory_abs = 100;
        ctx.inventory_base = -100;

        let call = make_call(100_000_000, 1000);
        let (_, exec_size, flags) = compute_execution(&ctx, &call).unwrap();

        assert_eq!(exec_size, 0);
        assert_eq!(flags, FLAG_VALID | FLAG_PARTIAL_OK);
    }

    #[test]
    fn test_vamm_validation_rejects_zero_liquidity() {
        let mut ctx = default_vamm_ctx();
        ctx.liquidity_notional_e6 = 0;

        assert!(ctx.validate().is_err());
    }

    #[test]
    fn test_passive_allows_zero_liquidity() {
        let ctx = default_passive_ctx();
        assert!(ctx.validate().is_ok());
    }

    #[test]
    fn test_validation_rejects_high_max_bps() {
        let mut ctx = default_vamm_ctx();
        ctx.max_total_bps = 9500;

        assert!(ctx.validate().is_err());
    }

    #[test]
    fn test_validation_rejects_fee_exceeds_max() {
        let mut ctx = default_vamm_ctx();
        ctx.trading_fee_bps = 100;
        ctx.base_spread_bps = 150;
        ctx.max_total_bps = 200;

        assert!(ctx.validate().is_err());
    }

    #[test]
    fn test_validation_rejects_zero_lp_pda() {
        let mut ctx = default_vamm_ctx();
        ctx.lp_pda = [0; 32];

        assert!(ctx.validate().is_err());
    }

    #[test]
    fn test_ctx_serialization_roundtrip() {
        let ctx = default_vamm_ctx();
        let mut buf = [0u8; CTX_VAMM_LEN];

        ctx.write_to(&mut buf).unwrap();
        let ctx2 = MatcherCtx::read_from(&buf).unwrap();

        assert_eq!(ctx.magic, ctx2.magic);
        assert_eq!(ctx.version, ctx2.version);
        assert_eq!(ctx.kind, ctx2.kind);
        assert_eq!(ctx.lp_pda, ctx2.lp_pda);
        assert_eq!(ctx.trading_fee_bps, ctx2.trading_fee_bps);
        assert_eq!(ctx.base_spread_bps, ctx2.base_spread_bps);
        assert_eq!(ctx.max_total_bps, ctx2.max_total_bps);
        assert_eq!(ctx.impact_k_bps, ctx2.impact_k_bps);
        assert_eq!(ctx.liquidity_notional_e6, ctx2.liquidity_notional_e6);
        assert_eq!(ctx.max_fill_abs, ctx2.max_fill_abs);
        assert_eq!(ctx.max_inventory_abs, ctx2.max_inventory_abs);
    }

    #[test]
    fn test_init_params_encode_decode() {
        let params = InitParams {
            kind: MatcherKind::Vamm as u8,
            trading_fee_bps: 5,
            base_spread_bps: 10,
            max_total_bps: 200,
            impact_k_bps: 100,
            liquidity_notional_e6: 1_000_000_000_000,
            max_fill_abs: 1_000_000_000,
            max_inventory_abs: 500_000,
        };

        let encoded = params.encode();
        let decoded = InitParams::parse(&encoded).unwrap();

        assert_eq!(params.kind, decoded.kind);
        assert_eq!(params.trading_fee_bps, decoded.trading_fee_bps);
        assert_eq!(params.base_spread_bps, decoded.base_spread_bps);
        assert_eq!(params.max_total_bps, decoded.max_total_bps);
        assert_eq!(params.impact_k_bps, decoded.impact_k_bps);
        assert_eq!(params.liquidity_notional_e6, decoded.liquidity_notional_e6);
        assert_eq!(params.max_fill_abs, decoded.max_fill_abs);
        assert_eq!(params.max_inventory_abs, decoded.max_inventory_abs);
    }
}
