#![no_std]

extern crate alloc;

pub mod passive_lp_matcher;

pub use passive_lp_matcher::*;

use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
    program_error::ProgramError,
    pubkey::Pubkey,
};

/// Matcher call instruction layout (67 bytes)
/// Offset  Field               Type     Size
/// 0       tag                 u8       1      Always 0
/// 1-9     req_id              u64      8
/// 9-11    lp_idx              u16      2
/// 11-19   lp_account_id       u64      8
/// 19-27   oracle_price_e6     u64      8
/// 27-43   req_size            i128     16
/// 43-67   reserved            [u8;24]  24
pub const MATCHER_CALL_LEN: usize = 67;
pub const MATCHER_CALL_TAG: u8 = 0;

/// Matcher return layout (64 bytes) - written to context account
pub const MATCHER_RETURN_LEN: usize = 64;
pub const FLAG_VALID: u32 = 1;
pub const FLAG_PARTIAL_OK: u32 = 2;
pub const FLAG_REJECTED: u32 = 4;
pub const MATCHER_ABI_VERSION: u32 = 1;

/// Matcher return structure written to context account
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MatcherReturn {
    pub abi_version: u32,
    pub flags: u32,
    pub exec_price_e6: u64,
    pub exec_size: i128,
    pub req_id: u64,
    pub lp_account_id: u64,
    pub oracle_price_e6: u64,
    pub reserved: u64,
}

impl MatcherReturn {
    /// Write to context account data (first 64 bytes)
    pub fn write_to(&self, data: &mut [u8]) -> Result<(), ProgramError> {
        if data.len() < MATCHER_RETURN_LEN {
            return Err(ProgramError::AccountDataTooSmall);
        }
        data[0..4].copy_from_slice(&self.abi_version.to_le_bytes());
        data[4..8].copy_from_slice(&self.flags.to_le_bytes());
        data[8..16].copy_from_slice(&self.exec_price_e6.to_le_bytes());
        data[16..32].copy_from_slice(&self.exec_size.to_le_bytes());
        data[32..40].copy_from_slice(&self.req_id.to_le_bytes());
        data[40..48].copy_from_slice(&self.lp_account_id.to_le_bytes());
        data[48..56].copy_from_slice(&self.oracle_price_e6.to_le_bytes());
        data[56..64].copy_from_slice(&self.reserved.to_le_bytes());
        Ok(())
    }

    pub fn rejected(req_id: u64, lp_account_id: u64, oracle_price_e6: u64) -> Self {
        Self {
            abi_version: MATCHER_ABI_VERSION,
            flags: FLAG_VALID | FLAG_REJECTED,
            exec_price_e6: 1, // Non-zero to pass validation
            exec_size: 0,
            req_id,
            lp_account_id,
            oracle_price_e6,
            reserved: 0,
        }
    }

    pub fn filled(
        exec_price: u64,
        exec_size: i128,
        req_id: u64,
        lp_account_id: u64,
        oracle_price_e6: u64,
    ) -> Self {
        Self {
            abi_version: MATCHER_ABI_VERSION,
            flags: FLAG_VALID,
            exec_price_e6: exec_price,
            exec_size,
            req_id,
            lp_account_id,
            oracle_price_e6,
            reserved: 0,
        }
    }

    pub fn zero_fill(req_id: u64, lp_account_id: u64, oracle_price_e6: u64) -> Self {
        Self {
            abi_version: MATCHER_ABI_VERSION,
            flags: FLAG_VALID | FLAG_PARTIAL_OK,
            exec_price_e6: 1, // Non-zero to pass validation
            exec_size: 0,
            req_id,
            lp_account_id,
            oracle_price_e6,
            reserved: 0,
        }
    }
}

/// Parsed matcher call from instruction data
#[derive(Clone, Copy, Debug)]
pub struct MatcherCall {
    pub req_id: u64,
    pub lp_idx: u16,
    pub lp_account_id: u64,
    pub oracle_price_e6: u64,
    pub req_size: i128,
}

impl MatcherCall {
    pub fn parse(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() < MATCHER_CALL_LEN {
            return Err(ProgramError::InvalidInstructionData);
        }
        if data[0] != MATCHER_CALL_TAG {
            return Err(ProgramError::InvalidInstructionData);
        }

        let req_id = u64::from_le_bytes(data[1..9].try_into().unwrap());
        let lp_idx = u16::from_le_bytes(data[9..11].try_into().unwrap());
        let lp_account_id = u64::from_le_bytes(data[11..19].try_into().unwrap());
        let oracle_price_e6 = u64::from_le_bytes(data[19..27].try_into().unwrap());
        let req_size = i128::from_le_bytes(data[27..43].try_into().unwrap());

        // Verify reserved bytes are zero
        for &b in &data[43..67] {
            if b != 0 {
                return Err(ProgramError::InvalidInstructionData);
            }
        }

        Ok(Self {
            req_id,
            lp_idx,
            lp_account_id,
            oracle_price_e6,
            req_size,
        })
    }
}

/// Process the matcher CPI call
///
/// Accounts:
/// 0. `[signer]` LP PDA (derived by percolator, passed as signer via invoke_signed)
/// 1. `[writable]` Matcher context account (owned by this program, result written here)
pub fn process_instruction(
    _program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    let account_iter = &mut accounts.iter();
    let _lp_pda = next_account_info(account_iter)?; // signer, not used by passive matcher
    let ctx_account = next_account_info(account_iter)?; // writable, write result here

    let call = MatcherCall::parse(instruction_data)?;

    // Use default config (50 bps edge)
    let cfg = PassiveMatcherConfig::default();

    // For a stateless matcher, we don't track inventory across calls
    // The LP's actual position is tracked by percolator's RiskEngine
    let mut lp_state = PassiveLpState::default();

    let matcher = PassiveOracleBpsMatcher;
    let result = matcher.execute_match(
        &cfg,
        &mut lp_state,
        call.oracle_price_e6,
        call.req_size,
        None, // No limit price in CPI interface
    );

    let ret = match result.reason {
        Reason::Ok => {
            if result.exec.size == 0 {
                MatcherReturn::zero_fill(call.req_id, call.lp_account_id, call.oracle_price_e6)
            } else {
                MatcherReturn::filled(
                    result.exec.price,
                    result.exec.size,
                    call.req_id,
                    call.lp_account_id,
                    call.oracle_price_e6,
                )
            }
        }
        _ => MatcherReturn::rejected(call.req_id, call.lp_account_id, call.oracle_price_e6),
    };

    // Write result to context account (first 64 bytes)
    let mut ctx_data = ctx_account.try_borrow_mut_data()?;
    ret.write_to(&mut ctx_data)?;

    Ok(())
}

#[cfg(not(feature = "no-entrypoint"))]
mod entrypoint {
    #[allow(unused_imports)]
    use alloc::format; // Required by entrypoint! macro in SBF builds
    use solana_program::{account_info::AccountInfo, entrypoint, entrypoint::ProgramResult, pubkey::Pubkey};
    use crate::process_instruction as processor;

    entrypoint!(process_instruction);

    fn process_instruction(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        instruction_data: &[u8],
    ) -> ProgramResult {
        processor(program_id, accounts, instruction_data)
    }
}
