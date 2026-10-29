use fujimod_emulator_cpu::{
    Access, AccessRefusalError, Cpu, DataAccess, ExecutionError, Exit, Hook, HookRange, Model,
    PhysicalAddress, Register, Run, arm,
};
use fujimod_emulator_cpu_unicorn::{Core, Library};
use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
};

#[derive(Clone, Copy)]
enum RefusalOrder {
    BeforeNestedRun,
    AfterNestedRun,
}

struct Probe {
    cpu: *const Core,
    order: RefusalOrder,
    refuse_inner: bool,
    outer_refusal: Cell<Option<Result<(), AccessRefusalError>>>,
    inner_refusal: Cell<Option<Result<(), AccessRefusalError>>>,
    instruction_refusal: Cell<Option<Result<(), AccessRefusalError>>>,
    inner_run: RefCell<Option<Result<Exit, ExecutionError>>>,
}

fn one_instruction(begin: u64) -> Run {
    Run {
        begin,
        until: None,
        watchdog_us: 0,
        instructions: Some(1),
    }
}

unsafe fn memory(_: Access, address: u64, _: i32, _: i64, data: *mut c_void) {
    let probe = unsafe { &*data.cast::<Probe>() };
    let cpu = unsafe { &*probe.cpu };
    match address {
        0x4000 => {
            if matches!(probe.order, RefusalOrder::BeforeNestedRun) {
                probe.outer_refusal.set(Some(cpu.refuse_access()));
            }
            let result = cpu.run(one_instruction(0x2000));
            *probe.inner_run.borrow_mut() = Some(result);
            if matches!(probe.order, RefusalOrder::AfterNestedRun) {
                probe.outer_refusal.set(Some(cpu.refuse_access()));
            }
        }
        0x5000 if probe.refuse_inner => {
            probe.inner_refusal.set(Some(cpu.refuse_access()));
        }
        _ => {}
    }
}

unsafe fn instruction(_: u64, _: u32, data: *mut c_void) {
    let probe = unsafe { &*data.cast::<Probe>() };
    probe
        .instruction_refusal
        .set(Some(unsafe { &*probe.cpu }.refuse_access()));
}

fn register(cpu: &Core, register: Register) -> u32 {
    let mut bytes = [0; 4];
    cpu.read_register(register, &mut bytes).unwrap();
    u32::from_le_bytes(bytes)
}

fn word(cpu: &Core, address: u64) -> u32 {
    let mut bytes = [0; 4];
    cpu.read_memory(address, &mut bytes).unwrap();
    u32::from_le_bytes(bytes)
}

fn nested_refusal(order: RefusalOrder, refuse_inner: bool) {
    let cpu = Library::linked()
        .unwrap()
        .open(Model::CortexR5F, 1024 * 1024)
        .unwrap();
    cpu.map_zeroed(0x1000, 0x2000, 7).unwrap();
    cpu.map_zeroed(0x4000, 0x2000, 3).unwrap();
    cpu.write_memory(0x1000, &0xe5810000u32.to_le_bytes())
        .unwrap(); // str r0, [r1]
    cpu.write_memory(0x2000, &0xe5932000u32.to_le_bytes())
        .unwrap(); // ldr r2, [r3]
    cpu.write_memory(0x5000, &0x22222222u32.to_le_bytes())
        .unwrap();
    for (register, value) in [
        (arm::CORE[0], 0x11111111u32),
        (arm::CORE[1], 0x4000),
        (arm::CORE[2], 0x33333333),
        (arm::CORE[3], 0x5000),
    ] {
        cpu.write_register(register, &value.to_le_bytes()).unwrap();
    }
    let mut probe = Box::new(Probe {
        cpu: &cpu,
        order,
        refuse_inner,
        outer_refusal: Cell::new(None),
        inner_refusal: Cell::new(None),
        instruction_refusal: Cell::new(None),
        inner_run: RefCell::new(None),
    });
    let data = (&mut *probe as *mut Probe).cast();
    let memory_hook = unsafe {
        cpu.install_hook(Hook::Memory {
            callback: memory,
            data,
            read: true,
            write: true,
            range: HookRange::All,
        })
    }
    .unwrap();
    let instruction_hook = unsafe {
        cpu.install_hook(Hook::Instruction {
            callback: instruction,
            data,
            range: HookRange::Inclusive {
                begin: 0x2000,
                end: 0x2000,
            },
        })
    }
    .unwrap();

    let result = cpu.run(one_instruction(0x1000));
    assert_eq!(probe.outer_refusal.get(), Some(Ok(())));
    assert_eq!(
        probe.instruction_refusal.get(),
        Some(Err(AccessRefusalError::OutsideMemoryHook))
    );
    assert!(
        matches!(
            result,
            Err(ExecutionError::AccessRefused {
                access: DataAccess::Write,
                address: PhysicalAddress(0x4000),
            })
        ),
        "{result:?}"
    );
    let inner_run = probe.inner_run.borrow();
    if refuse_inner {
        assert_eq!(probe.inner_refusal.get(), Some(Ok(())));
        assert!(
            matches!(
                &*inner_run,
                Some(Err(ExecutionError::AccessRefused {
                    access: DataAccess::Read,
                    address: PhysicalAddress(0x5000),
                }))
            ),
            "{inner_run:?}"
        );
        assert_eq!(register(&cpu, arm::CORE[2]), 0x33333333);
    } else {
        assert!(
            matches!(&*inner_run, Some(Ok(Exit::Stopped))),
            "{inner_run:?}"
        );
        assert_eq!(register(&cpu, arm::CORE[2]), 0x22222222);
    }
    assert_eq!(register(&cpu, Register::Pc), 0x1000);
    assert_eq!(word(&cpu, 0x4000), 0);
    assert_eq!(word(&cpu, 0x5000), 0x22222222);
    assert_eq!(
        cpu.refuse_access(),
        Err(AccessRefusalError::OutsideMemoryHook)
    );

    cpu.remove_hook(memory_hook).unwrap();
    cpu.remove_hook(instruction_hook).unwrap();
    cpu.run(one_instruction(0x1000)).unwrap();
    assert_eq!(word(&cpu, 0x4000), 0x11111111);
}

#[test]
fn refusal_before_nested_run_preserves_outer_denial_and_error_metadata() {
    for refuse_inner in [false, true] {
        nested_refusal(RefusalOrder::BeforeNestedRun, refuse_inner);
    }
}

#[test]
fn refusal_after_nested_run_restores_outer_memory_hook_scope() {
    for refuse_inner in [false, true] {
        nested_refusal(RefusalOrder::AfterNestedRun, refuse_inner);
    }
}
