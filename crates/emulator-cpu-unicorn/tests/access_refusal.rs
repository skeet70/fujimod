//! Covers the per-access engine contract only. Whole-transfer preflight of
//! LDM/STM/LDRD/STRD belongs to the native frontend.
//!
//! Instruction words were assembled with GNU as 2.46 (`-march=armv7-r`).
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ffi::c_void;

use fujimod_emulator_cpu::{
    Access, AccessRefusalError, Cpu, ExecutionError, Exit, Hook, HookRange, InstructionDisposition,
    Mmio, Model, PhysicalAddress, Register, Run, Translation, TranslationHooks, arm,
};
use fujimod_emulator_cpu_unicorn::{Core, Library};

const CODE: u64 = 0x1000;
const RAM: u64 = 0x4000;
const RAM_BYTES: usize = 0x2000;
const DEVICE: u64 = 0x8000;
const RAM_FILL: u8 = 0xaa;
const RAM_WORD: u32 = u32::from_le_bytes([RAM_FILL; 4]);
const UNTOUCHED: u32 = 0x7777_7777;

const STR_R0_R1_POST_4: u32 = 0xe481_0004; // str r0, [r1], #4
const LDR_R2_R1_MINUS_4: u32 = 0xe511_2004; // ldr r2, [r1, #-4]
const MOV_R3_1: u32 = 0xe3a0_3001; // mov r3, #1
const LDR_R0_R1_POST_4: u32 = 0xe491_0004; // ldr r0, [r1], #4
const STMIA_R2_WB_R0_R1: u32 = 0xe8a2_0003; // stmia r2!, {r0, r1}
const LDMIA_R2_WB_R0_R1: u32 = 0xe8b2_0003; // ldm r2!, {r0, r1}
const LDMIA_R2_R0_R1_R3: u32 = 0xe892_000b; // ldm r2, {r0, r1, r3}
const STMIA_R2_R0_R1: u32 = 0xe882_0003; // stm r2, {r0, r1}
const STR_R0_R1: u32 = 0xe581_0000; // str r0, [r1]

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Target {
    access: Access,
    address: u64,
    size: i32,
}

impl Target {
    fn word(access: Access, address: u64) -> Self {
        Self {
            access,
            address,
            size: 4,
        }
    }
}

#[derive(Clone, Copy)]
enum Action {
    Refuse,
    RestartThenRefuse,
    Restart,
}

/// An armed action fires at its target's first matching access only; an
/// engine retry of that access is admitted.
struct Admission<'a> {
    cpu: &'a Core,
    armed: Cell<Option<(Target, Action)>>,
    refusal_errors: RefCell<Vec<AccessRefusalError>>,
    restart_errors: RefCell<Vec<String>>,
}

impl<'a> Admission<'a> {
    fn install(cpu: &'a Core) -> Box<Self> {
        let admission = Box::new(Self {
            cpu,
            armed: Cell::new(None),
            refusal_errors: RefCell::default(),
            restart_errors: RefCell::default(),
        });
        unsafe {
            cpu.install_hook(Hook::Memory {
                callback: admit,
                data: (&*admission as *const Self).cast_mut().cast(),
                read: true,
                write: true,
                range: HookRange::All,
            })
        }
        .expect("install admission hook");
        admission
    }

    fn arm(&self, target: Target, action: Action) {
        self.armed.set(Some((target, action)));
    }

    fn assert_no_hook_errors(&self) {
        assert_eq!(*self.refusal_errors.borrow(), Vec::<AccessRefusalError>::new());
        assert_eq!(*self.restart_errors.borrow(), Vec::<String>::new());
    }
}

unsafe fn admit(access: Access, address: u64, size: i32, _: i64, data: *mut c_void) {
    let admission = unsafe { &*data.cast::<Admission<'_>>() };
    let current = Target {
        access,
        address,
        size,
    };
    let Some((target, action)) = admission.armed.get() else {
        return;
    };
    if target != current {
        return;
    }
    admission.armed.set(None);
    if matches!(action, Action::Restart | Action::RestartThenRefuse)
        && let Err(error) = admission.cpu.request_restart()
    {
        admission.restart_errors.borrow_mut().push(error);
    }
    if matches!(action, Action::Refuse | Action::RestartThenRefuse)
        && let Err(error) = admission.cpu.refuse_access()
    {
        admission.refusal_errors.borrow_mut().push(error);
    }
}

fn machine(program: &[u32]) -> Core {
    let cpu = Library::linked()
        .expect("pinned Unicorn build")
        .open(Model::CortexR5F, 4 * 1024 * 1024)
        .expect("open Cortex-R5F");
    cpu.map_zeroed(CODE, 0x1000, 7).unwrap();
    cpu.map_zeroed(RAM, RAM_BYTES, 3).unwrap();
    cpu.write_memory(RAM, &[RAM_FILL; RAM_BYTES]).unwrap();
    let code: Vec<u8> = program.iter().copied().flat_map(u32::to_le_bytes).collect();
    cpu.write_memory(CODE, &code).unwrap();
    for register in &arm::CORE[..4] {
        set(&cpu, *register, UNTOUCHED);
    }
    cpu
}

fn run(cpu: &Core, instructions: u64) -> Result<Exit, ExecutionError> {
    cpu.run(Run {
        begin: CODE,
        until: Some(CODE + 4 * instructions),
        watchdog_us: 0,
        instructions: None,
    })
}

fn set(cpu: &Core, register: Register, value: u32) {
    cpu.write_register(register, &value.to_le_bytes()).unwrap();
}

fn reg(cpu: &Core, register: Register) -> u32 {
    let mut bytes = [0; 4];
    cpu.read_register(register, &mut bytes).unwrap();
    u32::from_le_bytes(bytes)
}

fn word(cpu: &Core, address: u64) -> u32 {
    u32::from_le_bytes(bytes(cpu, address))
}

fn bytes<const N: usize>(cpu: &Core, address: u64) -> [u8; N] {
    let mut bytes = [0; N];
    cpu.read_memory(address, &mut bytes).unwrap();
    bytes
}

fn assert_refused(result: Result<Exit, ExecutionError>, target: Target) {
    match result {
        Err(ExecutionError::AccessRefused { access, address }) => {
            assert_eq!(Access::from(access), target.access);
            assert_eq!(address, PhysicalAddress(target.address));
        }
        other => panic!("expected refusal of {target:?}, got {other:?}"),
    }
}

#[test]
fn admitted_transfers_store_load_write_back_and_advance() {
    let cpu = machine(&[STR_R0_R1_POST_4, LDR_R2_R1_MINUS_4]);
    let admission = Admission::install(&cpu);
    set(&cpu, arm::CORE[0], 0x1111_1111);
    set(&cpu, arm::CORE[1], 0x4ffc);

    run(&cpu, 2).unwrap();

    admission.assert_no_hook_errors();
    assert_eq!(word(&cpu, 0x4ffc), 0x1111_1111);
    assert_eq!(reg(&cpu, arm::CORE[1]), 0x5000);
    assert_eq!(reg(&cpu, arm::CORE[2]), 0x1111_1111);
    assert_eq!(reg(&cpu, arm::PC), 0x1008);
}

unsafe fn record(access: Access, address: u64, size: i32, _: i64, data: *mut c_void) {
    let log = unsafe { &*data.cast::<RefCell<Vec<Target>>>() };
    log.borrow_mut().push(Target {
        access,
        address,
        size,
    });
}

/// Guards a refused store that still commits, writes back its base or reaches
/// a memory hook installed after the refusing one.
#[test]
fn refused_store_has_no_effect_and_stops_at_the_instruction() {
    let cpu = machine(&[STR_R0_R1_POST_4, MOV_R3_1]);
    let admission = Admission::install(&cpu);
    let later = RefCell::new(Vec::new());
    unsafe {
        cpu.install_hook(Hook::Memory {
            callback: record,
            data: (&later as *const RefCell<Vec<Target>>).cast_mut().cast(),
            read: true,
            write: true,
            range: HookRange::All,
        })
    }
    .unwrap();
    set(&cpu, arm::CORE[0], 0x1111_1111);
    set(&cpu, arm::CORE[1], 0x4ffc);
    let store = Target::word(Access::Write, 0x4ffc);
    admission.arm(store, Action::Refuse);

    assert_refused(run(&cpu, 2), store);

    admission.assert_no_hook_errors();
    assert_eq!(*later.borrow(), Vec::<Target>::new());
    assert_eq!(word(&cpu, 0x4ffc), RAM_WORD);
    assert_eq!(reg(&cpu, arm::CORE[1]), 0x4ffc);
    assert_eq!(reg(&cpu, arm::CORE[3]), UNTOUCHED);
    assert_eq!(reg(&cpu, arm::PC), 0x1000);
}

#[test]
fn refused_load_leaves_destination_and_base_and_stops_at_the_instruction() {
    let cpu = machine(&[LDR_R0_R1_POST_4, MOV_R3_1]);
    let admission = Admission::install(&cpu);
    cpu.write_memory(0x4ffc, &0x1111_1111u32.to_le_bytes())
        .unwrap();
    set(&cpu, arm::CORE[1], 0x4ffc);
    let load = Target::word(Access::Read, 0x4ffc);
    admission.arm(load, Action::Refuse);

    assert_refused(run(&cpu, 2), load);

    admission.assert_no_hook_errors();
    assert_eq!(reg(&cpu, arm::CORE[0]), UNTOUCHED);
    assert_eq!(reg(&cpu, arm::CORE[1]), 0x4ffc);
    assert_eq!(reg(&cpu, arm::CORE[3]), UNTOUCHED);
    assert_eq!(reg(&cpu, arm::PC), 0x1000);
}

#[test]
fn refusing_the_second_stm_word_keeps_the_first_word_and_skips_writeback() {
    let cpu = machine(&[STMIA_R2_WB_R0_R1]);
    let admission = Admission::install(&cpu);
    set(&cpu, arm::CORE[0], 0x1111_1111);
    set(&cpu, arm::CORE[1], 0x2222_2222);
    set(&cpu, arm::CORE[2], 0x4ff8);
    let second = Target::word(Access::Write, 0x4ffc);
    admission.arm(second, Action::Refuse);

    assert_refused(run(&cpu, 1), second);

    admission.assert_no_hook_errors();
    assert_eq!(word(&cpu, 0x4ff8), 0x1111_1111);
    assert_eq!(word(&cpu, 0x4ffc), RAM_WORD);
    assert_eq!(reg(&cpu, arm::CORE[2]), 0x4ff8);
    assert_eq!(reg(&cpu, arm::PC), 0x1000);
}

#[test]
fn refusing_the_second_ldm_word_keeps_the_first_register_and_skips_writeback() {
    let cpu = machine(&[LDMIA_R2_WB_R0_R1]);
    let admission = Admission::install(&cpu);
    cpu.write_memory(0x4ff8, &[0x11, 0x11, 0x11, 0x11, 0x22, 0x22, 0x22, 0x22])
        .unwrap();
    set(&cpu, arm::CORE[2], 0x4ff8);
    let second = Target::word(Access::Read, 0x4ffc);
    admission.arm(second, Action::Refuse);

    assert_refused(run(&cpu, 1), second);

    admission.assert_no_hook_errors();
    assert_eq!(reg(&cpu, arm::CORE[0]), 0x1111_1111);
    assert_eq!(reg(&cpu, arm::CORE[1]), UNTOUCHED);
    assert_eq!(reg(&cpu, arm::CORE[2]), 0x4ff8);
    assert_eq!(reg(&cpu, arm::PC), 0x1000);
}

#[derive(Default)]
struct Fifo {
    queue: RefCell<VecDeque<u32>>,
    writes: RefCell<Vec<(u64, u32, u64)>>,
}

impl Fifo {
    fn map(&self, cpu: &Core) {
        unsafe {
            cpu.map_mmio(
                DEVICE,
                0x1000,
                Mmio {
                    read: fifo_pop,
                    write: fifo_log,
                    data: (self as *const Self).cast_mut().cast(),
                },
            )
        }
        .unwrap();
    }
}

unsafe fn fifo_pop(_: u64, _: u32, data: *mut c_void) -> u64 {
    let fifo = unsafe { &*data.cast::<Fifo>() };
    fifo.queue.borrow_mut().pop_front().map_or(0, u64::from)
}

unsafe fn fifo_log(offset: u64, size: u32, value: u64, data: *mut c_void) {
    let fifo = unsafe { &*data.cast::<Fifo>() };
    fifo.writes.borrow_mut().push((offset, size, value));
}

#[test]
fn refused_device_read_pops_neither_the_refused_nor_later_entries() {
    let cpu = machine(&[LDMIA_R2_R0_R1_R3]);
    let fifo = Fifo::default();
    fifo.queue
        .borrow_mut()
        .extend([0x1111_1111, 0x2222_2222, 0x3333_3333]);
    fifo.map(&cpu);
    let admission = Admission::install(&cpu);
    set(&cpu, arm::CORE[2], DEVICE as u32);
    let second = Target::word(Access::Read, DEVICE + 4);
    admission.arm(second, Action::Refuse);

    assert_refused(run(&cpu, 1), second);

    admission.assert_no_hook_errors();
    assert_eq!(*fifo.queue.borrow(), [0x2222_2222, 0x3333_3333]);
    assert_eq!(reg(&cpu, arm::CORE[0]), 0x1111_1111);
    assert_eq!(reg(&cpu, arm::CORE[1]), UNTOUCHED);
    assert_eq!(reg(&cpu, arm::CORE[3]), UNTOUCHED);
    assert_eq!(reg(&cpu, arm::PC), 0x1000);
}

#[test]
fn refused_device_write_delivers_neither_the_refused_nor_later_words() {
    let cpu = machine(&[STMIA_R2_R0_R1]);
    let fifo = Fifo::default();
    fifo.map(&cpu);
    let admission = Admission::install(&cpu);
    set(&cpu, arm::CORE[0], 0x1111_1111);
    set(&cpu, arm::CORE[1], 0x2222_2222);
    set(&cpu, arm::CORE[2], DEVICE as u32);
    let first = Target::word(Access::Write, DEVICE);
    admission.arm(first, Action::Refuse);

    assert_refused(run(&cpu, 1), first);

    admission.assert_no_hook_errors();
    assert_eq!(*fifo.writes.borrow(), Vec::<(u64, u32, u64)>::new());
    assert_eq!(reg(&cpu, arm::PC), 0x1000);
}

/// Guards a page-crossing store whose byte split writes some bytes before the
/// refusal applies.
#[test]
fn refused_unaligned_store_writes_no_byte_and_the_same_store_later_writes_all() {
    let cpu = machine(&[STR_R0_R1]);
    let admission = Admission::install(&cpu);
    set(&cpu, arm::CORE[0], 0x4433_2211);
    set(&cpu, arm::CORE[1], 0x4ffe);
    let unaligned = Target::word(Access::Write, 0x4ffe);
    admission.arm(unaligned, Action::Refuse);

    assert_refused(run(&cpu, 1), unaligned);

    admission.assert_no_hook_errors();
    assert_eq!(bytes::<8>(&cpu, 0x4ffc), [RAM_FILL; 8]);
    assert_eq!(reg(&cpu, arm::PC), 0x1000);

    run(&cpu, 1).unwrap();

    assert_eq!(
        bytes::<8>(&cpu, 0x4ffc),
        [RAM_FILL, RAM_FILL, 0x11, 0x22, 0x33, 0x44, RAM_FILL, RAM_FILL]
    );
    assert_eq!(reg(&cpu, arm::PC), 0x1004);
}

unsafe fn execute_all(_: *mut c_void, _: u64, _: u32) -> Translation {
    Translation {
        instruction: InstructionDisposition::Execute,
        observe_block: false,
    }
}

unsafe fn record_restart(data: *mut c_void, pc: u64) {
    let restarts = unsafe { &*data.cast::<RefCell<Vec<u64>>>() };
    restarts.borrow_mut().push(pc);
}

/// Guards a pending restart that replays the refused store, or that survives
/// the refused run and restarts the next one.
#[test]
fn refusal_overrides_a_pending_restart_and_leaves_the_next_run_clean() {
    let cpu = machine(&[STR_R0_R1_POST_4, MOV_R3_1]);
    let restarts = RefCell::new(Vec::new());
    unsafe {
        cpu.translation_hooks(Some(TranslationHooks {
            classify: execute_all,
            restart: record_restart,
            data: (&restarts as *const RefCell<Vec<u64>>).cast_mut().cast(),
        }))
    }
    .unwrap();
    let admission = Admission::install(&cpu);
    set(&cpu, arm::CORE[0], 0x1111_1111);
    set(&cpu, arm::CORE[1], 0x4ffc);
    let store = Target::word(Access::Write, 0x4ffc);
    admission.arm(store, Action::RestartThenRefuse);

    assert_refused(run(&cpu, 2), store);

    admission.assert_no_hook_errors();
    assert_eq!(*restarts.borrow(), Vec::<u64>::new());
    assert_eq!(word(&cpu, 0x4ffc), RAM_WORD);
    assert_eq!(reg(&cpu, arm::CORE[1]), 0x4ffc);
    assert_eq!(reg(&cpu, arm::CORE[3]), UNTOUCHED);
    assert_eq!(reg(&cpu, arm::PC), 0x1000);

    run(&cpu, 2).unwrap();

    assert_eq!(*restarts.borrow(), Vec::<u64>::new());
    assert_eq!(word(&cpu, 0x4ffc), 0x1111_1111);
    assert_eq!(reg(&cpu, arm::CORE[1]), 0x5000);
    assert_eq!(reg(&cpu, arm::CORE[3]), 1);
    assert_eq!(reg(&cpu, arm::PC), 0x1008);

    cpu.write_memory(0x4ffc, &RAM_WORD.to_le_bytes()).unwrap();
    set(&cpu, arm::CORE[0], 0x2222_2222);
    set(&cpu, arm::CORE[1], 0x4ffc);
    admission.arm(store, Action::Restart);

    run(&cpu, 2).unwrap();

    admission.assert_no_hook_errors();
    assert_eq!(*restarts.borrow(), [0x1000]);
    assert_eq!(word(&cpu, 0x4ffc), 0x2222_2222);
    assert_eq!(reg(&cpu, arm::CORE[1]), 0x5000);
    assert_eq!(reg(&cpu, arm::PC), 0x1008);
}

struct InstructionHookRefusal<'a> {
    cpu: &'a Core,
    result: RefCell<Option<Result<(), AccessRefusalError>>>,
}

unsafe fn refuse_from_instruction_hook(_: u64, _: u32, data: *mut c_void) {
    let context = unsafe { &*data.cast::<InstructionHookRefusal<'_>>() };
    let mut result = context.result.borrow_mut();
    if result.is_none() {
        *result = Some(context.cpu.refuse_access());
    }
}

#[test]
fn refusal_outside_a_memory_hook_fails_and_leaves_execution_unchanged() {
    let cpu = machine(&[STR_R0_R1_POST_4]);
    assert_eq!(
        cpu.refuse_access(),
        Err(AccessRefusalError::OutsideMemoryHook)
    );
    let context = InstructionHookRefusal {
        cpu: &cpu,
        result: RefCell::new(None),
    };
    unsafe {
        cpu.install_hook(Hook::Instruction {
            callback: refuse_from_instruction_hook,
            data: (&context as *const InstructionHookRefusal<'_>)
                .cast_mut()
                .cast(),
            range: HookRange::All,
        })
    }
    .unwrap();
    set(&cpu, arm::CORE[0], 0x1111_1111);
    set(&cpu, arm::CORE[1], 0x4ffc);

    run(&cpu, 1).unwrap();

    assert_eq!(
        context.result.take(),
        Some(Err(AccessRefusalError::OutsideMemoryHook))
    );
    assert_eq!(word(&cpu, 0x4ffc), 0x1111_1111);
    assert_eq!(reg(&cpu, arm::CORE[1]), 0x5000);
    assert_eq!(reg(&cpu, arm::PC), 0x1004);
}
