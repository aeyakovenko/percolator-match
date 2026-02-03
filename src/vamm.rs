//! Configurable Matcher for Percolator Markets
//!
//! Supports multiple matcher modes:
//! - Passive: Fixed spread around oracle price
//! - vAMM: Spread + impact pricing with configurable liquidity curve
//!
//! LPs configure their preferred matcher mode and parameters via InitCtx.

use solana_program::{
    account_info::AccountInfo,
    entrypoint::ProgramResult,
    program_error::ProgramError,
};

use crate::{
    MatcherCall, MatcherReturn,
    CTX_VAMM_OFFSET, CTX_VAMM_LEN, MATCHER_CONTEXT_LEN,
    FLAG_VALID, FLAG_PARTIAL_OK,
};

// =============================================================================
// Matcher Mode
// =============================================================================

/// Matcher mode selector
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatcherMode {
    /// Passive: Fixed spread around oracle price (like legacy PassiveOracleBpsMatcher)
    Passive = 0,
    /// vAMM: Spread + impact pricing with configurable liquidity curve
    Vamm = 1,
}

impl TryFrom<u8> for MatcherMode {
    type Error = ProgramError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(MatcherMode::Passive),
            1 => Ok(MatcherMode::Vamm),
            _ => Err(ProgramError::InvalidInstructionData),
        }
    }
}

// =============================================================================
// Matcher Context Structure
// =============================================================================

/// Magic number for initialized context ("PERCMATC" in ASCII)
pub const VAMM_MAGIC: u64 = 0x5045_5243_4d41_5443;
/// Current context version
pub const VAMM_VERSION: u32 = 2;

/// Matcher context stored at offset 64 in matcher context account
///
/// Total size: 256 bytes (fits in CTX_VAMM_LEN)
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VammCtx {
    /// Magic number for initialization check
    pub magic: u64,                     // 8 bytes
    /// Version number
    pub version: u32,                   // 4 bytes
    /// Matcher mode: 0 = Passive, 1 = vAMM
    pub mode: u8,                       // 1 byte
    pub _pad0: [u8; 3],                 // 3 bytes (alignment)

    // ---- Fee Parameters (charged on every fill) ----
    /// Trading fee in basis points (e.g., 5 = 0.05%)
    /// This fee is added to the spread/impact
    pub trading_fee_bps: u32,           // 4 bytes

    // ---- Spread/Pricing Parameters ----
    /// Base spread in basis points (e.g., 10 = 0.10%)
    pub base_spread_bps: u32,           // 4 bytes
    /// Maximum total bps (spread + impact + fee) cap (e.g., 200 = 2.00%)
    pub max_total_bps: u32,             // 4 bytes
    /// Impact curvature multiplier (bps at size == liquidity) - vAMM only
    pub impact_k_bps: u32,              // 4 bytes

    /// Quoting depth in notional-e6 units (must be non-zero for vAMM)
    pub liquidity_notional_e6: u128,    // 16 bytes
    /// Maximum |exec_size| per call in base units (0 = allow 0 fill only)
    pub max_fill_abs: u128,             // 16 bytes

    // ---- State (updated each fill) ----
    /// LP inventory in base units (sign = LP position)
    pub inventory_base: i128,           // 16 bytes
    /// Last oracle price seen
    pub last_oracle_price_e6: u64,      // 8 bytes
    /// Last execution price
    pub last_exec_price_e6: u64,        // 8 bytes

    // ---- Inventory Limits (optional) ----
    /// Maximum absolute inventory (0 = no limit)
    pub max_inventory_abs: u128,        // 16 bytes

    /// Reserved for future use (fills to 256 bytes)
    pub _reserved: [u8; 144],           // 144 bytes
}

// Compile-time size check
const _: () = assert!(core::mem::size_of::<VammCtx>() == CTX_VAMM_LEN);

impl Default for VammCtx {
    fn default() -> Self {
        Self {
            magic: 0,
            version: 0,
            mode: 0,
            _pad0: [0; 3],
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
            _reserved: [0; 144],
        }
    }
}

impl VammCtx {
    /// Check if context is initialized with magic
    pub fn is_initialized(data: &[u8]) -> bool {
        if data.len() < 8 {
            return false;
        }
        let magic = u64::from_le_bytes(data[0..8].try_into().unwrap());
        magic == VAMM_MAGIC
    }

    /// Read context from data slice (at offset 64 in full context)
    pub fn read_from(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() < CTX_VAMM_LEN {
            return Err(ProgramError::AccountDataTooSmall);
        }

        let magic = u64::from_le_bytes(data[0..8].try_into().unwrap());
        if magic != VAMM_MAGIC {
            return Err(ProgramError::UninitializedAccount);
        }

        let version = u32::from_le_bytes(data[8..12].try_into().unwrap());
        let mode = data[12];
        let trading_fee_bps = u32::from_le_bytes(data[16..20].try_into().unwrap());
        let base_spread_bps = u32::from_le_bytes(data[20..24].try_into().unwrap());
        let max_total_bps = u32::from_le_bytes(data[24..28].try_into().unwrap());
        let impact_k_bps = u32::from_le_bytes(data[28..32].try_into().unwrap());
        let liquidity_notional_e6 = u128::from_le_bytes(data[32..48].try_into().unwrap());
        let max_fill_abs = u128::from_le_bytes(data[48..64].try_into().unwrap());
        let inventory_base = i128::from_le_bytes(data[64..80].try_into().unwrap());
        let last_oracle_price_e6 = u64::from_le_bytes(data[80..88].try_into().unwrap());
        let last_exec_price_e6 = u64::from_le_bytes(data[88..96].try_into().unwrap());
        let max_inventory_abs = u128::from_le_bytes(data[96..112].try_into().unwrap());

        let mut reserved = [0u8; 144];
        reserved.copy_from_slice(&data[112..256]);

        Ok(Self {
            magic,
            version,
            mode,
            _pad0: [0; 3],
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

    /// Write context to data slice (at offset 64 in full context)
    pub fn write_to(&self, data: &mut [u8]) -> Result<(), ProgramError> {
        if data.len() < CTX_VAMM_LEN {
            return Err(ProgramError::AccountDataTooSmall);
        }

        data[0..8].copy_from_slice(&self.magic.to_le_bytes());
        data[8..12].copy_from_slice(&self.version.to_le_bytes());
        data[12] = self.mode;
        data[13..16].copy_from_slice(&self._pad0);
        data[16..20].copy_from_slice(&self.trading_fee_bps.to_le_bytes());
        data[20..24].copy_from_slice(&self.base_spread_bps.to_le_bytes());
        data[24..28].copy_from_slice(&self.max_total_bps.to_le_bytes());
        data[28..32].copy_from_slice(&self.impact_k_bps.to_le_bytes());
        data[32..48].copy_from_slice(&self.liquidity_notional_e6.to_le_bytes());
        data[48..64].copy_from_slice(&self.max_fill_abs.to_le_bytes());
        data[64..80].copy_from_slice(&self.inventory_base.to_le_bytes());
        data[80..88].copy_from_slice(&self.last_oracle_price_e6.to_le_bytes());
        data[88..96].copy_from_slice(&self.last_exec_price_e6.to_le_bytes());
        data[96..112].copy_from_slice(&self.max_inventory_abs.to_le_bytes());
        data[112..256].copy_from_slice(&self._reserved);

        Ok(())
    }

    /// Get the matcher mode
    pub fn get_mode(&self) -> Result<MatcherMode, ProgramError> {
        MatcherMode::try_from(self.mode)
    }

    /// Validate context parameters
    pub fn validate(&self) -> Result<(), ProgramError> {
        // Validate mode
        let mode = self.get_mode()?;

        // For vAMM mode, liquidity_notional_e6 MUST be non-zero
        if mode == MatcherMode::Vamm && self.liquidity_notional_e6 == 0 {
            return Err(ProgramError::InvalidAccountData);
        }

        // max_total_bps MUST be <= 9000 (so 10000 - max_total_bps stays positive)
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

        Ok(())
    }
}

// =============================================================================
// InitCtx Instruction Layout (Tag 2)
// =============================================================================
// Offset  Field                   Type    Size
// 0       tag                     u8      1     Always 2
// 1       mode                    u8      1     0=Passive, 1=vAMM
// 2-6     trading_fee_bps         u32     4
// 6-10    base_spread_bps         u32     4
// 10-14   max_total_bps           u32     4
// 14-18   impact_k_bps            u32     4     (vAMM only)
// 18-34   liquidity_notional_e6   u128    16    (vAMM only, can be 0 for Passive)
// 34-50   max_fill_abs            u128    16
// 50-66   max_inventory_abs       u128    16    (0 = no limit)
// Total: 66 bytes

pub const INIT_VAMM_LEN: usize = 66;

/// Parsed InitCtx instruction
#[derive(Clone, Copy, Debug)]
pub struct InitVammParams {
    pub mode: u8,
    pub trading_fee_bps: u32,
    pub base_spread_bps: u32,
    pub max_total_bps: u32,
    pub impact_k_bps: u32,
    pub liquidity_notional_e6: u128,
    pub max_fill_abs: u128,
    pub max_inventory_abs: u128,
}

impl InitVammParams {
    pub fn parse(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() < INIT_VAMM_LEN {
            return Err(ProgramError::InvalidInstructionData);
        }
        if data[0] != crate::MATCHER_INIT_VAMM_TAG {
            return Err(ProgramError::InvalidInstructionData);
        }

        let mode = data[1];
        let trading_fee_bps = u32::from_le_bytes(data[2..6].try_into().unwrap());
        let base_spread_bps = u32::from_le_bytes(data[6..10].try_into().unwrap());
        let max_total_bps = u32::from_le_bytes(data[10..14].try_into().unwrap());
        let impact_k_bps = u32::from_le_bytes(data[14..18].try_into().unwrap());
        let liquidity_notional_e6 = u128::from_le_bytes(data[18..34].try_into().unwrap());
        let max_fill_abs = u128::from_le_bytes(data[34..50].try_into().unwrap());
        let max_inventory_abs = u128::from_le_bytes(data[50..66].try_into().unwrap());

        Ok(Self {
            mode,
            trading_fee_bps,
            base_spread_bps,
            max_total_bps,
            impact_k_bps,
            liquidity_notional_e6,
            max_fill_abs,
            max_inventory_abs,
        })
    }

    /// Encode InitCtx instruction data
    pub fn encode(&self) -> [u8; INIT_VAMM_LEN] {
        let mut data = [0u8; INIT_VAMM_LEN];
        data[0] = crate::MATCHER_INIT_VAMM_TAG;
        data[1] = self.mode;
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
// vAMM Instruction Processing
// =============================================================================

/// Process InitCtx instruction (Tag 2)
///
/// Initializes the matcher context with the specified parameters.
/// Can only be called once (context must be uninitialized).
///
/// Accounts:
/// 0. `[writable]` Matcher context account (owned by this program)
pub fn process_init_vamm(
    program_id: &solana_program::pubkey::Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    use solana_program::account_info::next_account_info;

    let account_iter = &mut accounts.iter();
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
    let params = InitVammParams::parse(instruction_data)?;

    // Validate mode
    let _ = MatcherMode::try_from(params.mode)?;

    // Check not already initialized
    {
        let data = ctx_account.try_borrow_data()?;
        if VammCtx::is_initialized(&data[CTX_VAMM_OFFSET..]) {
            return Err(ProgramError::AccountAlreadyInitialized);
        }
    }

    // Create and validate context
    let ctx = VammCtx {
        magic: VAMM_MAGIC,
        version: VAMM_VERSION,
        mode: params.mode,
        _pad0: [0; 3],
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
        _reserved: [0; 144],
    };

    ctx.validate()?;

    // Write context
    let mut data = ctx_account.try_borrow_mut_data()?;
    ctx.write_to(&mut data[CTX_VAMM_OFFSET..])?;

    Ok(())
}

/// Process matcher call
///
/// Computes execution price using the configured mode (Passive or vAMM),
/// updates inventory, and writes result to context.
pub fn process_vamm_call(
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
        // Avoid abs overflow
        return Err(ProgramError::InvalidInstructionData);
    }

    // Read context
    let mut ctx = {
        let data = ctx_account.try_borrow_data()?;
        VammCtx::read_from(&data[CTX_VAMM_OFFSET..])?
    };
    ctx.validate()?;

    // Compute execution based on mode
    let (exec_price, exec_size, flags) = compute_execution(&ctx, &call)?;

    // Update context state if fill occurred
    if exec_size != 0 {
        // LP takes opposite side: LP inventory -= exec_size
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

/// Compute execution price and size based on matcher mode
///
/// Returns (exec_price_e6, exec_size, flags)
fn compute_execution(
    ctx: &VammCtx,
    call: &MatcherCall,
) -> Result<(u64, i128, u32), ProgramError> {
    let mode = ctx.get_mode()?;

    match mode {
        MatcherMode::Passive => compute_passive_execution(ctx, call),
        MatcherMode::Vamm => compute_vamm_execution(ctx, call),
    }
}

/// Compute passive execution (fixed spread around oracle)
fn compute_passive_execution(
    ctx: &VammCtx,
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

    // Passive mode: total_bps = base_spread_bps + trading_fee_bps
    let base = ctx.base_spread_bps as u128;
    let fee = ctx.trading_fee_bps as u128;
    let max_total = ctx.max_total_bps as u128;
    let total_bps = core::cmp::min(max_total, base + fee);

    // Compute exec_price based on direction
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
    ctx: &VammCtx,
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
    // abs_notional_e6 = (fill_abs * oracle_price_e6) / 1_000_000
    let oracle = call.oracle_price_e6 as u128;
    let abs_notional_e6 = fill_abs
        .checked_mul(oracle)
        .ok_or(ProgramError::ArithmeticOverflow)?
        / 1_000_000u128;

    // Compute impact in bps
    // impact_bps = (abs_notional_e6 * impact_k_bps) / liquidity_notional_e6
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

    // Compute exec_price based on direction
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

/// Check and enforce inventory limit, reducing fill size if needed
fn check_inventory_limit(ctx: &VammCtx, fill_abs: u128, is_buy: bool) -> Result<u128, ProgramError> {
    // If no limit, return full fill
    if ctx.max_inventory_abs == 0 {
        return Ok(fill_abs);
    }

    // LP inventory change: LP sells when user buys, LP buys when user sells
    // is_buy => LP inventory decreases (more short)
    // !is_buy => LP inventory increases (more long)

    let current_inv = ctx.inventory_base;
    let max_inv = ctx.max_inventory_abs as i128;

    // Calculate new inventory after trade
    let inv_delta = if is_buy {
        -(fill_abs as i128)
    } else {
        fill_abs as i128
    };

    let new_inv = current_inv.saturating_add(inv_delta);

    // If within limits, allow full fill
    if new_inv.unsigned_abs() <= ctx.max_inventory_abs {
        return Ok(fill_abs);
    }

    // Calculate maximum allowed fill to stay within limits
    if is_buy {
        // LP going more short, limit is -max_inv
        if current_inv <= -max_inv {
            // Already at limit
            return Ok(0);
        }
        // Max fill = current_inv - (-max_inv) = current_inv + max_inv
        let max_fill = (current_inv + max_inv).unsigned_abs();
        Ok(core::cmp::min(fill_abs, max_fill))
    } else {
        // LP going more long, limit is +max_inv
        if current_inv >= max_inv {
            // Already at limit
            return Ok(0);
        }
        // Max fill = max_inv - current_inv
        let max_fill = (max_inv - current_inv).unsigned_abs();
        Ok(core::cmp::min(fill_abs, max_fill))
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn default_vamm_ctx() -> VammCtx {
        VammCtx {
            magic: VAMM_MAGIC,
            version: VAMM_VERSION,
            mode: MatcherMode::Vamm as u8,
            _pad0: [0; 3],
            trading_fee_bps: 5,         // 0.05% fee
            base_spread_bps: 10,        // 0.10%
            max_total_bps: 200,         // 2.00% cap
            impact_k_bps: 100,          // bps at size == liquidity
            liquidity_notional_e6: 1_000_000_000_000, // 1M in e6
            max_fill_abs: 1_000_000_000, // 1000 base units
            inventory_base: 0,
            last_oracle_price_e6: 0,
            last_exec_price_e6: 0,
            max_inventory_abs: 0,       // No limit
            _reserved: [0; 144],
        }
    }

    fn default_passive_ctx() -> VammCtx {
        VammCtx {
            magic: VAMM_MAGIC,
            version: VAMM_VERSION,
            mode: MatcherMode::Passive as u8,
            _pad0: [0; 3],
            trading_fee_bps: 5,         // 0.05% fee
            base_spread_bps: 50,        // 0.50%
            max_total_bps: 200,         // 2.00% cap
            impact_k_bps: 0,            // Not used in passive mode
            liquidity_notional_e6: 0,   // Not used in passive mode
            max_fill_abs: 1_000_000_000,
            inventory_base: 0,
            last_oracle_price_e6: 0,
            last_exec_price_e6: 0,
            max_inventory_abs: 0,
            _reserved: [0; 144],
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

        // With 10 bps spread + 5 bps fee = 15 bps minimum
        // exec_price >= 100_000_000 * 10015 / 10000 = 100_150_000
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

        // Passive: 50 bps spread + 5 bps fee = 55 bps
        // exec_price = 100_000_000 * 10055 / 10000 = 100_550_000
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
        ctx.max_inventory_abs = 100; // Limit to 100 units
        ctx.inventory_base = 0;

        // User buys 1000 => LP sells 1000 => LP would go to -1000
        // But limit is 100, so fill capped to 100
        let call = make_call(100_000_000, 1000);
        let (_, exec_size, _) = compute_execution(&ctx, &call).unwrap();

        assert_eq!(exec_size, 100);
    }

    #[test]
    fn test_inventory_limit_at_boundary() {
        let mut ctx = default_vamm_ctx();
        ctx.max_inventory_abs = 100;
        ctx.inventory_base = -100; // Already at short limit

        // User buys more => LP can't go more short
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
        // fee (100) + spread (150) = 250 > max (200)

        assert!(ctx.validate().is_err());
    }

    #[test]
    fn test_ctx_serialization_roundtrip() {
        let ctx = default_vamm_ctx();
        let mut buf = [0u8; CTX_VAMM_LEN];

        ctx.write_to(&mut buf).unwrap();
        let ctx2 = VammCtx::read_from(&buf).unwrap();

        assert_eq!(ctx.magic, ctx2.magic);
        assert_eq!(ctx.version, ctx2.version);
        assert_eq!(ctx.mode, ctx2.mode);
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
        let params = InitVammParams {
            mode: MatcherMode::Vamm as u8,
            trading_fee_bps: 5,
            base_spread_bps: 10,
            max_total_bps: 200,
            impact_k_bps: 100,
            liquidity_notional_e6: 1_000_000_000_000,
            max_fill_abs: 1_000_000_000,
            max_inventory_abs: 500_000,
        };

        let encoded = params.encode();
        let decoded = InitVammParams::parse(&encoded).unwrap();

        assert_eq!(params.mode, decoded.mode);
        assert_eq!(params.trading_fee_bps, decoded.trading_fee_bps);
        assert_eq!(params.base_spread_bps, decoded.base_spread_bps);
        assert_eq!(params.max_total_bps, decoded.max_total_bps);
        assert_eq!(params.impact_k_bps, decoded.impact_k_bps);
        assert_eq!(params.liquidity_notional_e6, decoded.liquidity_notional_e6);
        assert_eq!(params.max_fill_abs, decoded.max_fill_abs);
        assert_eq!(params.max_inventory_abs, decoded.max_inventory_abs);
    }
}
