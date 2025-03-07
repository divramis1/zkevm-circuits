use crate::{
    circuit_input_builder::{
        CircuitInputStateRef, CopyBytes, CopyDataType, CopyEvent, CopyEventStepsBuilder, ExecStep,
        NumberOrHash,
    },
    Error,
};
use eth_types::{
    evm_types::memory::{MemoryRange, MemoryWordRange},
    GethExecStep, Word, U256,
};
use ethers_core::utils::keccak256;

use super::Opcode;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Sha3;

impl Opcode for Sha3 {
    fn gen_associated_ops(
        state: &mut CircuitInputStateRef,
        geth_steps: &[GethExecStep],
    ) -> Result<Vec<ExecStep>, Error> {
        let geth_step = &geth_steps[0];
        let mut exec_step = state.new_step(geth_step)?;

        let offset = state.stack_pop(&mut exec_step)?;
        let size = state.stack_pop(&mut exec_step)?;
        #[cfg(feature = "enable-stack")]
        {
            assert_eq!(offset, geth_step.stack.nth_last(0)?);
            assert_eq!(size, geth_step.stack.nth_last(1)?);
        }

        if size.gt(&U256::zero()) {
            state
                .call_ctx_mut()?
                .memory
                .extend_at_least(offset.as_usize() + size.as_usize());
        }

        let sha3_input = state
            .call_ctx()?
            .memory
            .read_chunk(MemoryRange::new_with_length(
                offset.low_u64(),
                size.low_u64(),
            ));

        // keccak-256 hash of the given data in memory.
        let sha3 = keccak256(&sha3_input);
        let _output = Word::from_big_endian(&sha3);
        #[cfg(feature = "enable-stack")]
        assert_eq!(_output, geth_steps[1].stack.last()?);
        state.stack_push(&mut exec_step, sha3.into())?;

        // Memory read operations
        let rw_counter_start = state.block_ctx.rwc;

        let copy_steps = if size.as_usize() != 0 {
            let dst_range = MemoryWordRange::align_range(offset.low_u64(), size.low_u64());
            let mem = state.call_ctx()?.memory.read_chunk(dst_range);
            // Read step
            let mut chunk_index = dst_range.start_slot().0;
            for _ in 0..dst_range.word_count() {
                state.memory_read_word(&mut exec_step, chunk_index.into())?;
                chunk_index += 32;
            }

            CopyEventStepsBuilder::memory_range(dst_range)
                .source(mem.as_slice())
                .build()
        } else {
            vec![]
        };

        state.block.sha3_inputs.push(sha3_input);
        let call_id = state.call()?.call_id;
        state.push_copy(
            &mut exec_step,
            CopyEvent {
                src_addr: offset.low_u64(),
                src_addr_end: offset
                    .low_u64()
                    .checked_add(size.as_u64())
                    .unwrap_or(u64::MAX),
                src_type: CopyDataType::Memory,
                src_id: NumberOrHash::Number(call_id),
                dst_addr: 0,
                dst_type: CopyDataType::RlcAcc,
                dst_id: NumberOrHash::Number(call_id),
                log_id: None,
                rw_counter_start,
                copy_bytes: CopyBytes::new(copy_steps, None, None),
                access_list: vec![],
            },
        );

        Ok(vec![exec_step])
    }
}

#[cfg(any(feature = "test", test))]
pub mod sha3_tests {
    use eth_types::{
        bytecode,
        evm_types::{memory::MemoryWordRange, Memory, OpcodeId},
        geth_types::GethData,
        Bytecode, Word,
    };
    use ethers_core::utils::keccak256;
    use mock::{
        test_ctx::helpers::{account_0_code_account_1_no_code, tx_from_1_to_0},
        TestContext,
    };
    use rand::{random, Rng};

    use crate::{
        circuit_input_builder::{CircuitsParams, ExecState},
        mock::BlockData,
        operation::{MemoryOp, StackOp, RW},
    };

    /// Generate bytecode for SHA3 opcode after having populated sufficient
    /// memory given the offset and size arguments for SHA3.
    pub fn gen_sha3_code(offset: usize, size: usize, mem_kind: MemoryKind) -> (Bytecode, Vec<u8>) {
        let mut rng = rand::thread_rng();
        let data_len = match mem_kind {
            MemoryKind::LessThanSize => {
                offset
                    + if size.gt(&0) {
                        rng.gen_range(0..size)
                    } else {
                        0
                    }
            }
            MemoryKind::EqualToSize => offset + size,
            MemoryKind::MoreThanSize => {
                offset
                    + size
                    + if size.gt(&0) {
                        rng.gen_range(0..size)
                    } else {
                        0
                    }
            }
            MemoryKind::Empty => 0,
        };
        let data = rand_bytes(data_len);
        let mut memory = Vec::with_capacity(data_len);

        // add opcodes to populate memory in the current context.
        let mut code = Bytecode::default();
        for (i, mem_chunk) in data.chunks(32).enumerate() {
            let mem_value = if mem_chunk.len() < 32 {
                std::iter::repeat(0u8)
                    .take(32 - mem_chunk.len())
                    .chain(mem_chunk.to_vec())
                    .collect::<Vec<u8>>()
            } else {
                mem_chunk.to_vec()
            };
            memory.extend_from_slice(&mem_value);
            code.op_mstore(32 * i, Word::from_big_endian(&mem_value));
        }
        // append SHA3 related opcodes at the tail end.
        let code_tail = bytecode! {
            PUSH32(size)
            PUSH32(offset)
            SHA3
            STOP
        };
        code.append(&code_tail);
        (code, memory)
    }

    /// Memory of a context with respect to the input size to SHA3.
    pub enum MemoryKind {
        /// Variant defining empty memory.
        Empty,
        /// Variant defining memory length being less than size.
        LessThanSize,
        /// Variant defining memory length being equal to size.
        EqualToSize,
        /// Variant defining memory length being more than size.
        MoreThanSize,
    }

    fn rand_bytes(size: usize) -> Vec<u8> {
        (0..size).map(|_| random()).collect::<Vec<u8>>()
    }

    fn test_ok(offset: usize, size: usize, mem_kind: MemoryKind) {
        let (code, memory) = gen_sha3_code(offset, size, mem_kind);

        // The memory that is hashed.
        let mut memory_view = memory
            .iter()
            .copied()
            .skip(offset)
            .take(size)
            .collect::<Vec<u8>>();
        memory_view.resize(size, 0);
        let expected_sha3_value = keccak256(&memory_view);

        let block: GethData = TestContext::<2, 1>::new(
            None,
            account_0_code_account_1_no_code(code),
            tx_from_1_to_0,
            |block, _txs| block,
        )
        .unwrap()
        .into();

        let mut builder = BlockData::new_from_geth_data_with_params(
            block.clone(),
            CircuitsParams {
                max_rws: 2048,
                ..Default::default()
            },
        )
        .new_circuit_input_builder();
        builder
            .handle_block(&block.eth_block, &block.geth_traces)
            .unwrap();

        let step = builder.block.txs()[0]
            .steps()
            .iter()
            .find(|step| step.exec_state == ExecState::Op(OpcodeId::SHA3))
            .unwrap();

        let call_id = builder.block.txs()[0].calls()[0].call_id;

        // stack read and write.
        assert_eq!(
            [0, 1, 2]
                .map(|idx| &builder.block.container.stack[step.bus_mapping_instance[idx].as_usize()])
                .map(|op| (op.rw(), op.op())),
            [
                (
                    RW::READ,
                    &StackOp::new(call_id, 1022.into(), Word::from(offset)),
                ),
                (
                    RW::READ,
                    &StackOp::new(call_id, 1023.into(), Word::from(size)),
                ),
                (
                    RW::WRITE,
                    &StackOp::new(call_id, 1023.into(), expected_sha3_value.into()),
                ),
            ]
        );

        // Initial memory_len bytes are the memory writes from MSTORE instruction, so we
        // skip them.
        let memory = Memory(memory);
        let dst_range = MemoryWordRange::align_range(offset, size);
        assert_eq!(
            builder
                .block
                .container
                .memory
                .iter()
                .rev()
                .take(dst_range.word_count())
                .rev()
                .map(|op| (op.rw(), op.op().clone()))
                .collect::<Vec<(RW, MemoryOp)>>(),
            {
                let mut memory_ops = Vec::with_capacity(size);
                let mut chunk_index = dst_range.start_slot().0;
                for _ in 0..dst_range.word_count() {
                    let word = memory.read_word(chunk_index.into());
                    memory_ops.push((RW::READ, MemoryOp::new(call_id, chunk_index.into(), word)));
                    chunk_index += 32;
                }
                memory_ops
            },
        );

        let copy_events = builder.block.copy_events.clone();

        // single copy event with `size` reads and `size` writes.
        assert_eq!(copy_events.len(), 1);
        //assert_eq!(copy_events[0].bytes.len(), size);

        let mut mask_count = 0;
        for (idx, (value, is_code, is_mask)) in copy_events[0].copy_bytes.bytes.iter().enumerate() {
            if !is_mask {
                assert_eq!(Some(value), memory_view.get(idx - mask_count));
                assert!(!is_code);
            } else {
                mask_count += 1;
            }
        }
    }

    #[test]
    fn sha3_opcode_ok() {
        test_ok(0x10, 0x32, MemoryKind::Empty);
        test_ok(0x34, 0x44, MemoryKind::LessThanSize);
        test_ok(0x222, 0x111, MemoryKind::EqualToSize);
        test_ok(0x20, 0x30, MemoryKind::MoreThanSize);
    }
}
