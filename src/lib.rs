#![no_std]

extern crate alloc;

pub mod passive_lp_matcher;
pub mod vamm;

pub use passive_lp_matcher::*;
pub use vamm::*;

use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
    program_error::ProgramError,
    pubkey::Pubkey,
};

// =============================================================================
// Context Account Layout
// =============================================================================
// Bytes 0-63:   Matcher return data (64 bytes, written on each call) - ABI required
// Bytes 64-319: MatcherCtx state (256 bytes)
// Total: 320 bytes

/// Offset where matcher return is written (must be 0 per ABI)
pub const CTX_RETURN_OFFSET: usize = 0;
/// Length of matcher return (64 bytes per ABI)
pub const MATCHER_RETURN_LEN: usize = 64;
/// Offset where matcher context state begins
pub const CTX_VAMM_OFFSET: usize = MATCHER_RETURN_LEN; // 64
/// Length of matcher context state
pub const CTX_VAMM_LEN: usize = 256;
/// Minimum context account size
pub const MATCHER_CONTEXT_LEN: usize = 320;

// =============================================================================
// Instruction Tags
// =============================================================================

/// Matcher call instruction tag (from percolator CPI)
pub const MATCHER_CALL_TAG: u8 = 0;
/// Initialize context instruction tag
pub const MATCHER_INIT_VAMM_TAG: u8 = 2;

// =============================================================================
// Matcher Call Layout (67 bytes) - Tag 0
// =============================================================================
/// Offset  Field               Type     Size
/// 0       tag                 u8       1      Always 0
/// 1-9     req_id              u64      8
/// 9-11    lp_idx              u16      2
/// 11-19   lp_account_id       u64      8
/// 19-27   oracle_price_e6     u64      8
/// 27-43   req_size            i128     16
/// 43-67   reserved            [u8;24]  24
pub const MATCHER_CALL_LEN: usize = 67;

// =============================================================================
// Matcher Return Layout (64 bytes)
// =============================================================================

pub const FLAG_VALID: u32 = 1;
pub const FLAG_PARTIAL_OK: u32 = 2;
pub const FLAG_REJECTED: u32 = 4;
pub const MATCHER_ABI_VERSION: u32 = 1;

/// Matcher return structure written to context account at offset 0
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
    /// Write to context account data at offset 0 (ABI required)
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
            exec_price_e6: 1,
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
            exec_price_e6: 1,
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

// =============================================================================
// Instruction Processing
// =============================================================================

/// Process the matcher instruction
///
/// ## Instructions
///
/// ### Tag 0: Matcher Call (from percolator CPI)
/// Accounts:
/// 0. `[signer]` LP PDA (must match stored PDA in context)
/// 1. `[writable]` Matcher context account (must be initialized)
///
/// ### Tag 2: Initialize Context
/// Accounts:
/// 0. `[]` LP PDA (stored for signature verification)
/// 1. `[writable]` Matcher context account (owned by this program)
pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    if instruction_data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }

    match instruction_data[0] {
        MATCHER_CALL_TAG => process_matcher_call(program_id, accounts, instruction_data),
        MATCHER_INIT_VAMM_TAG => vamm::process_init(program_id, accounts, instruction_data),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

/// Process Matcher Call instruction (Tag 0)
///
/// Context MUST be initialized - uninitalized contexts are rejected.
/// LP PDA must be a signer and must match the stored PDA.
fn process_matcher_call(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    let account_iter = &mut accounts.iter();
    let lp_pda = next_account_info(account_iter)?;
    let ctx_account = next_account_info(account_iter)?;

    // Verify context account is owned by this program
    if ctx_account.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }

    // Verify minimum size
    if ctx_account.data_len() < MATCHER_CONTEXT_LEN {
        return Err(ProgramError::AccountDataTooSmall);
    }

    // Check if context is initialized
    let is_initialized = {
        let ctx_data = ctx_account.try_borrow_data()?;
        vamm::MatcherCtx::is_initialized(&ctx_data[CTX_VAMM_OFFSET..])
    };

    // MUST be initialized - reject uninitialized contexts
    // This prevents state manipulation by untrusted callers
    if !is_initialized {
        return Err(ProgramError::UninitializedAccount);
    }

    // Require LP PDA signature
    if !lp_pda.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    // Delegate to unified matcher processing (validates PDA match internally)
    vamm::process_call(lp_pda, ctx_account, instruction_data)
}

#[cfg(not(feature = "no-entrypoint"))]
mod entrypoint {
    #[allow(unused_imports)]
    use alloc::format;
    use crate::process_instruction as processor;
    use solana_program::{
        account_info::AccountInfo, entrypoint, entrypoint::ProgramResult, pubkey::Pubkey,
    };

    entrypoint!(process_instruction);

    fn process_instruction(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        instruction_data: &[u8],
    ) -> ProgramResult {
        processor(program_id, accounts, instruction_data)
    }
}
