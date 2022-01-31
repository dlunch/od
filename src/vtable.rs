use capstone::{arch::x86, RegId};
use std::collections::{btree_map::Entry, BTreeSet};

use anyhow::{anyhow, Result};
use object::{Object, ObjectSection};

use super::{
    context::{Context, Instruction},
    util::convert_pointer,
};

pub fn find_vtables(context: &mut Context<'_>) -> Result<Vec<u64>> {
    let text_section = context.object.section_by_name(".text").ok_or(anyhow!("No .text section"))?;

    let rdata_section = context.object.section_by_name(".rdata").ok_or(anyhow!("No .rdata section"))?;
    let rdata = rdata_section.data()?;

    // 1. Find vtable candidates
    struct State {
        last: Option<u64>,
        all: BTreeSet<u64>,
    }

    let vtable_candidates = rdata
        .windows(context.pointer_size)
        .enumerate()
        .step_by(context.pointer_size)
        .fold(
            State {
                last: None,
                all: BTreeSet::new(),
            },
            |mut state, (i, x)| {
                let ptr = convert_pointer(x, context.pointer_size);

                if text_section.address() < ptr && ptr < text_section.address() + text_section.size() {
                    if state.last.is_none() {
                        let addr = i as u64 + rdata_section.address();

                        log::trace!("vtable candidate at {:#x}", addr);
                        state.last = Some(addr);
                    }
                } else if state.last.is_some() {
                    state.all.insert(state.last.unwrap());
                    state.last = None
                }

                state
            },
        )
        .all;

    // 2. Validate vtable candidates by parsing the code.
    let mut vtables = BTreeSet::new();

    let mut it = context.insns.iter().peekable();
    while let Some(insn) = it.next() {
        // test if x64; lea reg, [rip + x]; mov [dest], reg
        if insn.mnemonic == x86::X86Insn::X86_INS_LEA {
            let operand_types = insn.operands.iter().map(|x| &x.op_type).collect::<Vec<_>>();

            if let [x86::X86OperandType::Reg(reg), x86::X86OperandType::Mem(mem)] = &operand_types[..] {
                if mem.base().0 as u32 == x86::X86Reg::X86_REG_RIP {
                    let src_addr = (mem.disp() + insn.address as i64) as u64 + insn.bytes.len() as u64; // TODO: check overflow

                    if vtable_candidates.contains(&src_addr) && is_mov_from_reg_to_mem(it.peek().unwrap(), reg)? {
                        log::debug!("Found vtable {:#x}", src_addr);

                        vtables.insert(src_addr);
                        if let Entry::Vacant(e) = context.xrefs.entry(src_addr) {
                            e.insert(Vec::new());
                        }
                        context.xrefs.get_mut(&src_addr).unwrap().push(insn.address);
                    }
                }
            }
        }
        // test if x86; mov dword ptr [reg], offset
        if insn.mnemonic == x86::X86Insn::X86_INS_MOV {
            let operand_types = insn.operands.iter().map(|x| &x.op_type).collect::<Vec<_>>();

            if let [x86::X86OperandType::Mem(_), x86::X86OperandType::Imm(imm)] = &operand_types[..] {
                let src_addr = *imm as u64;
                if vtable_candidates.contains(&src_addr) {
                    log::debug!("Found vtable {:#x}", imm);

                    vtables.insert(src_addr);
                    if let Entry::Vacant(e) = context.xrefs.entry(src_addr) {
                        e.insert(Vec::new());
                    }
                    context.xrefs.get_mut(&src_addr).unwrap().push(insn.address);
                }
            }
        }
    }

    Ok(vtables.into_iter().collect())
}

fn is_mov_from_reg_to_mem(insn: &Instruction, reg: &RegId) -> Result<bool> {
    if insn.mnemonic != x86::X86Insn::X86_INS_MOV {
        return Ok(false);
    }
    let operand_types = insn.operands.iter().map(|x| &x.op_type).collect::<Vec<_>>();

    if let [x86::X86OperandType::Mem(_), x86::X86OperandType::Reg(insn_reg)] = &operand_types[..] {
        if insn_reg == reg {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use tokio::fs;

    use super::{find_vtables, Context};

    fn init() {
        let mut builder = pretty_env_logger::formatted_builder();

        if let Ok(s) = ::std::env::var("RUST_LOG") {
            builder.parse_filters(&s);
        }

        let _ = builder.is_test(true).try_init();
    }

    #[tokio::test]
    async fn test_x86() -> anyhow::Result<()> {
        init();

        let file = fs::read("./test_data/msvc_rtti1_32.exe").await?;
        let obj = object::File::parse(&*file)?;
        let mut context = Context::new(obj)?;

        let vtables = find_vtables(&mut context)?;
        assert_eq!(vtables, [0x40e164, 0x40e16c, 0x40e174, 0x40e194, 0x40e1b0, 0x40ecb0,]);
        assert_eq!(*context.xrefs.get(&0x40e164).unwrap(), vec![0x40104a]);
        assert_eq!(*context.xrefs.get(&0x40e16c).unwrap(), vec![0x4010e6,]);
        assert_eq!(*context.xrefs.get(&0x40e174).unwrap(), vec![0x4013bf, 0x4013e5, 0x4013fc,]);
        assert_eq!(*context.xrefs.get(&0x40e194).unwrap(), vec![0x40135e, 0x40137c,]);
        assert_eq!(*context.xrefs.get(&0x40e1b0).unwrap(), vec![0x401391, 0x4013af,]);
        assert_eq!(*context.xrefs.get(&0x40ecb0).unwrap(), vec![0x403ae4, 0x403b02,],);
        Ok(())
    }

    #[tokio::test]
    async fn test_x64() -> anyhow::Result<()> {
        init();

        let file = fs::read("./test_data/msvc_rtti1_64.exe").await?;
        let obj = object::File::parse(&*file)?;
        let mut context = Context::new(obj)?;

        let vtables = find_vtables(&mut context)?;
        assert_eq!(vtables, [0x140010318, 0x140010338, 0x140010368, 0x140010390, 0x1400113a0]);
        assert_eq!(*context.xrefs.get(&0x140010318).unwrap(), vec![0x14000106a,]);
        assert_eq!(*context.xrefs.get(&0x140010338).unwrap(), vec![0x14000149c,]);
        assert_eq!(*context.xrefs.get(&0x140010368).unwrap(), vec![0x1400013d9, 0x1400013fc,],);
        assert_eq!(*context.xrefs.get(&0x140010390).unwrap(), vec![0x140001435, 0x140001458,],);
        assert_eq!(*context.xrefs.get(&0x1400113a0).unwrap(), vec![0x1400044dd, 0x140004500,],);

        Ok(())
    }
}
