//! Regression test: SMOD (and SDIV) with zero divisor loaded from memory.
//!
//! When the divisor is opaque to LLVM (e.g. loaded via MLOAD), the `srem i256`
//! instruction is UB for divisor==0. LLVM may exploit this to remove the zero
//! check entirely, producing incorrect results instead of the EVM-mandated 0.
//!
//! This test uses MLOAD to make the zero divisor opaque, so LLVM cannot
//! constant-fold it away. The JIT result is compared against the interpreter.

use super::{with_evm_context, DEF_SPEC};
use crate::{Backend, EvmCompiler};
use revm_bytecode::opcode as op;
use revm_interpreter::InstructionResult;
use revm_primitives::U256;

/// Build bytecode: MSTORE(a, 0), MSTORE(b, 32), MLOAD(32), MLOAD(0), <op>
///
/// The operands pass through MLOAD (an opaque builtin), preventing LLVM from
/// constant-folding or exploiting division-by-zero UB at compile time.
fn bytecode_binop_opaque(opcode: u8, a: U256, b: U256) -> Vec<u8> {
    let mut code = Vec::with_capacity(128);

    // Store a at memory[0]
    code.push(op::PUSH32);
    code.extend_from_slice(&a.to_be_bytes::<32>());
    code.push(op::PUSH1);
    code.push(0x00);
    code.push(op::MSTORE);

    // Store b at memory[32]
    code.push(op::PUSH32);
    code.extend_from_slice(&b.to_be_bytes::<32>());
    code.push(op::PUSH1);
    code.push(0x20);
    code.push(op::MSTORE);

    // Load b from memory (opaque to LLVM)
    code.push(op::PUSH1);
    code.push(0x20);
    code.push(op::MLOAD);

    // Load a from memory (opaque to LLVM)
    code.push(op::PUSH1);
    code.push(0x00);
    code.push(op::MLOAD);

    // Execute the target opcode: OP(a, b)
    code.push(opcode);

    // STOP (implicit from vec zero-init, but be explicit)
    code.push(op::STOP);

    code
}

fn run_opaque_binop_test<B: Backend>(
    compiler: &mut EvmCompiler<B>,
    name: &str,
    opcode: u8,
    a: U256,
    b: U256,
    expected: U256,
) {
    let code = bytecode_binop_opaque(opcode, a, b);

    unsafe { compiler.clear() }.unwrap();
    compiler.inspect_stack_length(true);
    let f = unsafe { compiler.jit(name, &code, DEF_SPEC) }.unwrap();

    with_evm_context(&code, |ecx, stack, stack_len| {
        let r = unsafe { f.call(Some(stack), Some(stack_len), ecx) };
        assert_eq!(r, InstructionResult::Stop, "{name}: unexpected return");
        assert_eq!(*stack_len, 1, "{name}: expected 1 stack element");
        let actual = stack.as_slice()[0].to_u256();
        assert_eq!(actual, expected, "{name}: JIT result mismatch (got {actual}, expected {expected})");
    });
}

// SMOD with zero divisor (opaque): EVM spec mandates result = 0
matrix_tests!(smod_zero_a5 = |jit| run_opaque_binop_test(
    jit, "smod_zero_a5", op::SMOD, U256::from(5), U256::ZERO, U256::ZERO
));

matrix_tests!(smod_zero_a0 = |jit| run_opaque_binop_test(
    jit, "smod_zero_a0", op::SMOD, U256::ZERO, U256::ZERO, U256::ZERO
));

// SDIV with zero divisor (opaque): EVM spec mandates result = 0
matrix_tests!(sdiv_zero_a5 = |jit| run_opaque_binop_test(
    jit, "sdiv_zero_a5", op::SDIV, U256::from(5), U256::ZERO, U256::ZERO
));

// DIV/MOD with zero divisor (opaque): verify builtins are also correct
matrix_tests!(div_zero_opaque = |jit| run_opaque_binop_test(
    jit, "div_zero_opaque", op::DIV, U256::from(32), U256::ZERO, U256::ZERO
));

matrix_tests!(mod_zero_opaque = |jit| run_opaque_binop_test(
    jit, "mod_zero_opaque", op::MOD, U256::from(32), U256::ZERO, U256::ZERO
));
