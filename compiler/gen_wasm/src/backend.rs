use parity_wasm::builder;
use parity_wasm::builder::{CodeLocation, FunctionDefinition, ModuleBuilder, SignatureBuilder};
use parity_wasm::elements::{
    BlockType, Instruction, Instruction::*, Instructions, Local, ValueType,
};

use roc_collections::all::MutMap;
use roc_module::low_level::LowLevel;
use roc_module::symbol::Symbol;
use roc_mono::ir::{CallType, Expr, JoinPointId, Literal, Proc, Stmt};
use roc_mono::layout::{Builtin, Layout};

use crate::code_builder::{CodeBuilder, VirtualMachineSymbolState};
use crate::layout::WasmLayout;
use crate::storage::{StackMemoryLocation, SymbolStorage};
use crate::{
    copy_memory, pop_stack_frame, push_stack_frame, round_up_to_alignment, CopyMemoryConfig,
    LocalId, ALIGN_1, ALIGN_2, ALIGN_4, ALIGN_8, PTR_SIZE, PTR_TYPE,
};

// Don't allocate any constant data at address zero or near it. Would be valid, but bug-prone.
// Follow Emscripten's example by using 1kB (4 bytes would probably do)
const UNUSED_DATA_SECTION_BYTES: u32 = 1024;

#[derive(Clone, Copy, Debug)]
struct LabelId(u32);

enum LocalKind {
    Parameter,
    Variable,
}

// TODO: use Bumpalo Vec once parity_wasm supports general iterators (>=0.43)
pub struct WasmBackend<'a> {
    // Module: Wasm AST
    pub module_builder: ModuleBuilder,

    // Module: internal state & IR mappings
    _data_offset_map: MutMap<Literal<'a>, u32>,
    _data_offset_next: u32,
    proc_symbol_map: MutMap<Symbol, CodeLocation>,

    // Functions: Wasm AST
    code_builder: CodeBuilder,
    arg_types: std::vec::Vec<ValueType>,
    locals: std::vec::Vec<Local>,

    // Functions: internal state & IR mappings
    stack_memory: i32,
    stack_frame_pointer: Option<LocalId>,
    symbol_storage_map: MutMap<Symbol, SymbolStorage>,
    /// how many blocks deep are we (used for jumps)
    block_depth: u32,
    joinpoint_label_map: MutMap<JoinPointId, (u32, std::vec::Vec<SymbolStorage>)>,
}

impl<'a> WasmBackend<'a> {
    pub fn new() -> Self {
        WasmBackend {
            // Module: Wasm AST
            module_builder: builder::module(),

            // Module: internal state & IR mappings
            _data_offset_map: MutMap::default(),
            _data_offset_next: UNUSED_DATA_SECTION_BYTES,
            proc_symbol_map: MutMap::default(),

            // Functions: Wasm AST
            code_builder: CodeBuilder::new(),
            arg_types: std::vec::Vec::with_capacity(8),
            locals: std::vec::Vec::with_capacity(32),

            // Functions: internal state & IR mappings
            stack_memory: 0,
            stack_frame_pointer: None,
            symbol_storage_map: MutMap::default(),
            block_depth: 0,
            joinpoint_label_map: MutMap::default(),
        }
    }

    fn reset(&mut self) {
        // Functions: Wasm AST
        self.code_builder.clear();
        self.arg_types.clear();
        self.locals.clear();

        // Functions: internal state & IR mappings
        self.stack_memory = 0;
        self.stack_frame_pointer = None;
        self.symbol_storage_map.clear();
        self.joinpoint_label_map.clear();
        assert_eq!(self.block_depth, 0);
    }

    /**********************************************************

            PROCEDURE

    ***********************************************************/

    pub fn build_proc(&mut self, proc: Proc<'a>, sym: Symbol) -> Result<u32, String> {
        // println!("\ngenerating procedure {:?}\n", sym);

        let signature_builder = self.start_proc(&proc);

        self.build_stmt(&proc.body, &proc.ret_layout)?;

        let function_def = self.finalize_proc(signature_builder);
        let location = self.module_builder.push_function(function_def);
        let function_index = location.body;
        self.proc_symbol_map.insert(sym, location);
        self.reset();
        // println!("\nfinished generating {:?}\n", sym);

        Ok(function_index)
    }

    fn start_proc(&mut self, proc: &Proc<'a>) -> SignatureBuilder {
        let ret_layout = WasmLayout::new(&proc.ret_layout);

        let signature_builder = if let WasmLayout::StackMemory { .. } = ret_layout {
            self.arg_types.push(PTR_TYPE);
            self.start_block(BlockType::NoResult); // block to ensure all paths pop stack memory (if any)
            builder::signature()
        } else {
            let ret_type = ret_layout.value_type();
            self.start_block(BlockType::Value(ret_type)); // block to ensure all paths pop stack memory (if any)
            builder::signature().with_result(ret_type)
        };

        for (layout, symbol) in proc.args {
            self.create_storage(&WasmLayout::new(layout), *symbol, LocalKind::Parameter);
        }

        signature_builder.with_params(self.arg_types.clone())
    }

    fn finalize_proc(&mut self, signature_builder: SignatureBuilder) -> FunctionDefinition {
        self.end_block(); // end the block from start_proc, to ensure all paths pop stack memory (if any)

        let mut final_instructions = Vec::with_capacity(self.code_builder.len() + 10);

        if self.stack_memory > 0 {
            push_stack_frame(
                &mut final_instructions,
                self.stack_memory,
                self.stack_frame_pointer.unwrap(),
            );
        }

        self.code_builder.finalize_into(&mut final_instructions);

        if self.stack_memory > 0 {
            pop_stack_frame(
                &mut final_instructions,
                self.stack_memory,
                self.stack_frame_pointer.unwrap(),
            );
        }
        final_instructions.push(End);

        builder::function()
            .with_signature(signature_builder.build_sig())
            .body()
            .with_locals(self.locals.clone())
            .with_instructions(Instructions::new(final_instructions))
            .build() // body
            .build() // function
    }

    /**********************************************************

            SYMBOL STORAGE, LOCALS, AND COPYING

    ***********************************************************/

    fn get_next_local_id(&self) -> LocalId {
        LocalId((self.arg_types.len() + self.locals.len()) as u32)
    }

    fn create_storage(
        &mut self,
        wasm_layout: &WasmLayout,
        symbol: Symbol,
        kind: LocalKind,
    ) -> SymbolStorage {
        let next_local_id = self.get_next_local_id();

        let storage = match wasm_layout {
            WasmLayout::Primitive(value_type, size) => match kind {
                LocalKind::Parameter => {
                    self.arg_types.push(*value_type);
                    SymbolStorage::Local {
                        local_id: next_local_id,
                        value_type: *value_type,
                        size: *size,
                    }
                }
                LocalKind::Variable => SymbolStorage::VirtualMachineStack {
                    vm_state: VirtualMachineSymbolState::NotYetPushed,
                    value_type: *value_type,
                    size: *size,
                },
            },

            WasmLayout::HeapMemory => {
                match kind {
                    LocalKind::Parameter => self.arg_types.push(PTR_TYPE),
                    LocalKind::Variable => self.locals.push(Local::new(1, PTR_TYPE)),
                }
                SymbolStorage::Local {
                    local_id: next_local_id,
                    value_type: PTR_TYPE,
                    size: PTR_SIZE,
                }
            }

            WasmLayout::StackMemory {
                size,
                alignment_bytes,
            } => {
                let location = match kind {
                    LocalKind::Parameter => {
                        self.arg_types.push(PTR_TYPE);
                        StackMemoryLocation::PointerArg(next_local_id)
                    }

                    LocalKind::Variable => {
                        if self.stack_frame_pointer.is_none() {
                            self.stack_frame_pointer = Some(next_local_id);
                            self.locals.push(Local::new(1, PTR_TYPE));
                        }

                        let offset =
                            round_up_to_alignment(self.stack_memory, *alignment_bytes as i32);

                        self.stack_memory = offset + (*size as i32);

                        StackMemoryLocation::FrameOffset(offset as u32)
                    }
                };

                SymbolStorage::StackMemory {
                    location,
                    size: *size,
                    alignment_bytes: *alignment_bytes,
                }
            }
        };

        self.symbol_storage_map.insert(symbol, storage.clone());

        storage
    }

    fn get_symbol_storage(&self, sym: &Symbol) -> &SymbolStorage {
        self.symbol_storage_map.get(sym).unwrap_or_else(|| {
            panic!(
                "Symbol {:?} not found in function scope:\n{:?}",
                sym, self.symbol_storage_map
            )
        })
    }

    /// Load symbols to the top of the VM stack
    /// (There is no method for one symbol. This is deliberate, since
    /// if anyone ever called it in a loop, it would generate inefficient code)
    fn load_symbols(&mut self, symbols: &[Symbol]) {
        if self.code_builder.verify_stack_match(symbols) {
            // The symbols were already at the top of the stack, do nothing!
            // This should be quite common due to the structure of the Mono IR
            return;
        }
        for sym in symbols.iter() {
            let storage = self.get_symbol_storage(sym).to_owned();
            match storage {
                SymbolStorage::VirtualMachineStack {
                    vm_state,
                    value_type,
                    size,
                } => {
                    let next_local_id = self.get_next_local_id();
                    let maybe_next_vm_state =
                        self.code_builder.load_symbol(*sym, vm_state, next_local_id);
                    match maybe_next_vm_state {
                        // The act of loading the value changed the VM state, so update it
                        Some(next_vm_state) => {
                            self.symbol_storage_map.insert(
                                *sym,
                                SymbolStorage::VirtualMachineStack {
                                    vm_state: next_vm_state,
                                    value_type,
                                    size,
                                },
                            );
                        }
                        None => {
                            // Loading the value required creating a new local, because
                            // it was not in a convenient position in the VM stack.
                            self.locals.push(Local::new(1, value_type));
                            self.symbol_storage_map.insert(
                                *sym,
                                SymbolStorage::Local {
                                    local_id: next_local_id,
                                    value_type,
                                    size,
                                },
                            );
                        }
                    }
                }
                SymbolStorage::Local { local_id, .. }
                | SymbolStorage::StackMemory {
                    location: StackMemoryLocation::PointerArg(local_id),
                    ..
                } => {
                    self.code_builder.push(GetLocal(local_id.0));
                    self.code_builder.set_top_symbol(*sym);
                }

                SymbolStorage::StackMemory {
                    location: StackMemoryLocation::FrameOffset(offset),
                    ..
                } => {
                    self.code_builder.extend(&[
                        GetLocal(self.stack_frame_pointer.unwrap().0),
                        I32Const(offset as i32),
                        I32Add,
                    ]);
                    self.code_builder.set_top_symbol(*sym);
                }
            }
        }
    }

    fn copy_symbol_to_memory(
        &mut self,
        to_ptr: LocalId,
        to_offset: u32,
        from_symbol: Symbol,
    ) -> u32 {
        let from_storage = self.get_symbol_storage(&from_symbol).to_owned();
        match from_storage {
            SymbolStorage::StackMemory {
                location,
                size,
                alignment_bytes,
            } => {
                let (from_ptr, from_offset) = location.local_and_offset(self.stack_frame_pointer);
                copy_memory(
                    &mut self.code_builder,
                    CopyMemoryConfig {
                        from_ptr,
                        from_offset,
                        to_ptr,
                        to_offset,
                        size,
                        alignment_bytes,
                    },
                );
                size
            }

            SymbolStorage::VirtualMachineStack {
                value_type, size, ..
            }
            | SymbolStorage::Local {
                value_type, size, ..
            } => {
                let store_instruction = match (value_type, size) {
                    (ValueType::I64, 8) => I64Store(ALIGN_8, to_offset),
                    (ValueType::I32, 4) => I32Store(ALIGN_4, to_offset),
                    (ValueType::I32, 2) => I32Store16(ALIGN_2, to_offset),
                    (ValueType::I32, 1) => I32Store8(ALIGN_1, to_offset),
                    (ValueType::F32, 4) => F32Store(ALIGN_4, to_offset),
                    (ValueType::F64, 8) => F64Store(ALIGN_8, to_offset),
                    _ => {
                        panic!("Cannot store {:?} with alignment of {:?}", value_type, size);
                    }
                };
                self.code_builder.push(GetLocal(to_ptr.0));
                self.load_symbols(&[from_symbol]);
                self.code_builder.push(store_instruction);
                size
            }
        }
    }

    /// generate code to copy a value from one SymbolStorage to another
    pub fn copy_value_by_storage(
        &mut self,
        to: &SymbolStorage,
        from: &SymbolStorage,
        from_symbol: Symbol,
    ) {
        use SymbolStorage::*;

        match (to, from) {
            (
                Local {
                    local_id: to_local_id,
                    value_type: to_value_type,
                    size: to_size,
                },
                VirtualMachineStack {
                    value_type: from_value_type,
                    size: from_size,
                    ..
                },
            ) => {
                debug_assert!(to_value_type == from_value_type);
                debug_assert!(to_size == from_size);
                self.load_symbols(&[from_symbol]);
                self.code_builder.push(SetLocal(to_local_id.0));
                self.symbol_storage_map.insert(from_symbol, to.clone());
            }

            (
                Local {
                    local_id: to_local_id,
                    value_type: to_value_type,
                    size: to_size,
                },
                Local {
                    local_id: from_local_id,
                    value_type: from_value_type,
                    size: from_size,
                },
            ) => {
                debug_assert!(to_value_type == from_value_type);
                debug_assert!(to_size == from_size);
                self.code_builder.push(GetLocal(from_local_id.0));
                self.code_builder.push(SetLocal(to_local_id.0));
            }

            (
                StackMemory {
                    location: to_location,
                    size: to_size,
                    alignment_bytes: to_alignment_bytes,
                },
                StackMemory {
                    location: from_location,
                    size: from_size,
                    alignment_bytes: from_alignment_bytes,
                },
            ) => {
                let (from_ptr, from_offset) =
                    from_location.local_and_offset(self.stack_frame_pointer);
                let (to_ptr, to_offset) = to_location.local_and_offset(self.stack_frame_pointer);
                debug_assert!(*to_size == *from_size);
                debug_assert!(*to_alignment_bytes == *from_alignment_bytes);
                copy_memory(
                    &mut self.code_builder,
                    CopyMemoryConfig {
                        from_ptr,
                        from_offset,
                        to_ptr,
                        to_offset,
                        size: *from_size,
                        alignment_bytes: *from_alignment_bytes,
                    },
                );
            }

            _ => {
                panic!("Cannot copy storage from {:?} to {:?}", from, to);
            }
        }
    }

    /// Ensure SymbolStorage has an associated local.
    /// (Blocks can't access the VM stack of their parent scope, but they can access locals.)
    fn ensure_symbol_storage_has_local(
        &mut self,
        symbol: Symbol,
        storage: SymbolStorage,
    ) -> SymbolStorage {
        if let SymbolStorage::VirtualMachineStack {
            vm_state,
            value_type,
            size,
        } = storage
        {
            let local_id = self.get_next_local_id();
            if vm_state != VirtualMachineSymbolState::NotYetPushed {
                self.code_builder.load_symbol(symbol, vm_state, local_id);
                self.code_builder.push(SetLocal(local_id.0));
            }

            self.locals.push(Local::new(1, value_type));
            let new_storage = SymbolStorage::Local {
                local_id,
                value_type,
                size,
            };

            self.symbol_storage_map.insert(symbol, new_storage.clone());
            return new_storage;
        } else {
            storage
        }
    }

    /**********************************************************

            STATEMENTS

    ***********************************************************/

    /// start a loop that leaves a value on the stack
    fn start_loop_with_return(&mut self, value_type: ValueType) {
        self.block_depth += 1;
        self.code_builder.push(Loop(BlockType::Value(value_type)));
    }

    fn start_block(&mut self, block_type: BlockType) {
        self.block_depth += 1;
        self.code_builder.push(Block(block_type));
    }

    fn end_block(&mut self) {
        self.block_depth -= 1;
        self.code_builder.push(End);
    }

    fn build_stmt(&mut self, stmt: &Stmt<'a>, ret_layout: &Layout<'a>) -> Result<(), String> {
        match stmt {
            // Simple optimisation: if we are just returning the expression, we don't need a local
            Stmt::Let(let_sym, expr, layout, Stmt::Ret(ret_sym)) if *let_sym == *ret_sym => {
                let wasm_layout = WasmLayout::new(layout);

                if let WasmLayout::StackMemory {
                    size,
                    alignment_bytes,
                } = wasm_layout
                {
                    // Map this symbol to the first argument (pointer into caller's stack)
                    // Saves us from having to copy it later
                    let storage = SymbolStorage::StackMemory {
                        location: StackMemoryLocation::PointerArg(LocalId(0)),
                        size,
                        alignment_bytes,
                    };
                    self.symbol_storage_map.insert(*let_sym, storage);
                }

                self.build_expr(let_sym, expr, layout)?;

                if let WasmLayout::Primitive(value_type, size) = wasm_layout {
                    let vm_state = self.code_builder.set_top_symbol(*let_sym);
                    self.symbol_storage_map.insert(
                        *let_sym,
                        SymbolStorage::VirtualMachineStack {
                            vm_state,
                            value_type,
                            size,
                        },
                    );
                }

                self.code_builder.push(Br(self.block_depth)); // jump to end of function (stack frame pop)
                Ok(())
            }

            Stmt::Let(sym, expr, layout, following) => {
                let wasm_layout = WasmLayout::new(layout);

                self.create_storage(&wasm_layout, *sym, LocalKind::Variable);
                self.build_expr(sym, expr, layout)?;

                if let WasmLayout::Primitive(value_type, size) = wasm_layout {
                    let vm_state = self.code_builder.set_top_symbol(*sym);
                    self.symbol_storage_map.insert(
                        *sym,
                        SymbolStorage::VirtualMachineStack {
                            vm_state,
                            value_type,
                            size,
                        },
                    );
                }

                self.build_stmt(following, ret_layout)?;
                Ok(())
            }

            Stmt::Ret(sym) => {
                use crate::storage::SymbolStorage::*;

                let storage = self.symbol_storage_map.get(sym).unwrap();

                match storage {
                    StackMemory {
                        location,
                        size,
                        alignment_bytes,
                    } => {
                        let (from_ptr, from_offset) =
                            location.local_and_offset(self.stack_frame_pointer);
                        copy_memory(
                            &mut self.code_builder,
                            CopyMemoryConfig {
                                from_ptr,
                                from_offset,
                                to_ptr: LocalId(0),
                                to_offset: 0,
                                size: *size,
                                alignment_bytes: *alignment_bytes,
                            },
                        );
                    }

                    _ => {
                        self.load_symbols(&[*sym]);
                        self.code_builder.push(Br(self.block_depth)); // jump to end of function (for stack frame pop)
                    }
                }

                Ok(())
            }

            Stmt::Switch {
                cond_symbol,
                cond_layout: _,
                branches,
                default_branch,
                ret_layout: _,
            } => {
                // NOTE currently implemented as a series of conditional jumps
                // We may be able to improve this in the future with `Select`
                // or `BrTable`

                // Ensure the condition value is not stored only in the VM stack
                // Otherwise we can't reach it from inside the block
                let cond_storage = self.get_symbol_storage(cond_symbol).to_owned();
                self.ensure_symbol_storage_has_local(*cond_symbol, cond_storage);

                // create (number_of_branches - 1) new blocks.
                for _ in 0..branches.len() {
                    self.start_block(BlockType::NoResult)
                }

                // then, we jump whenever the value under scrutiny is equal to the value of a branch
                for (i, (value, _, _)) in branches.iter().enumerate() {
                    // put the cond_symbol on the top of the stack
                    self.load_symbols(&[*cond_symbol]);

                    self.code_builder.push(I32Const(*value as i32));

                    // compare the 2 topmost values
                    self.code_builder.push(I32Eq);

                    // "break" out of `i` surrounding blocks
                    self.code_builder.push(BrIf(i as u32));
                }

                // if we never jumped because a value matched, we're in the default case
                self.build_stmt(default_branch.1, ret_layout)?;

                // now put in the actual body of each branch in order
                // (the first branch would have broken out of 1 block,
                // hence we must generate its code first)
                for (_, _, branch) in branches.iter() {
                    self.end_block();

                    self.build_stmt(branch, ret_layout)?;
                }

                Ok(())
            }
            Stmt::Join {
                id,
                parameters,
                body,
                remainder,
            } => {
                // make locals for join pointer parameters
                let mut jp_param_storages = std::vec::Vec::with_capacity(parameters.len());
                for parameter in parameters.iter() {
                    let wasm_layout = WasmLayout::new(&parameter.layout);
                    let mut param_storage =
                        self.create_storage(&wasm_layout, parameter.symbol, LocalKind::Variable);
                    param_storage =
                        self.ensure_symbol_storage_has_local(parameter.symbol, param_storage);
                    jp_param_storages.push(param_storage);
                }

                self.start_block(BlockType::NoResult);

                self.joinpoint_label_map
                    .insert(*id, (self.block_depth, jp_param_storages));

                self.build_stmt(remainder, ret_layout)?;

                self.end_block();

                // A `return` inside of a `loop` seems to make it so that the `loop` itself
                // also "returns" (so, leaves on the stack) a value of the return type.
                let return_wasm_layout = WasmLayout::new(ret_layout);
                self.start_loop_with_return(return_wasm_layout.value_type());

                self.build_stmt(body, ret_layout)?;

                // ends the loop
                self.end_block();

                Ok(())
            }
            Stmt::Jump(id, arguments) => {
                let (target, param_storages) = self.joinpoint_label_map[id].clone();

                for (arg_symbol, param_storage) in arguments.iter().zip(param_storages.iter()) {
                    let arg_storage = self.get_symbol_storage(arg_symbol).clone();
                    self.copy_value_by_storage(&param_storage, &arg_storage, *arg_symbol);
                }

                // jump
                let levels = self.block_depth - target;
                self.code_builder.push(Br(levels));

                Ok(())
            }
            x => Err(format!("statement not yet implemented: {:?}", x)),
        }
    }

    /**********************************************************

            EXPRESSIONS

    ***********************************************************/

    fn build_expr(
        &mut self,
        sym: &Symbol,
        expr: &Expr<'a>,
        layout: &Layout<'a>,
    ) -> Result<(), String> {
        match expr {
            Expr::Literal(lit) => self.load_literal(lit, layout),

            Expr::Call(roc_mono::ir::Call {
                call_type,
                arguments,
            }) => match call_type {
                CallType::ByName { name: func_sym, .. } => {
                    self.load_symbols(*arguments);
                    let function_location = self.proc_symbol_map.get(func_sym).ok_or(format!(
                        "Cannot find function {:?} called from {:?}",
                        func_sym, sym
                    ))?;

                    // TODO: Recreating the same WasmLayout as in the Let, for Backend compatibility
                    let wasm_layout = WasmLayout::new(layout);
                    let push = wasm_layout.stack_memory() == 0;
                    let pops = arguments.len();
                    self.code_builder.call(function_location.body, pops, push);
                    Ok(())
                }

                CallType::LowLevel { op: lowlevel, .. } => {
                    self.build_call_low_level(lowlevel, arguments, layout)
                }
                x => Err(format!("the call type, {:?}, is not yet implemented", x)),
            },

            Expr::Struct(fields) => self.create_struct(sym, layout, fields),

            x => Err(format!("Expression is not yet implemented {:?}", x)),
        }
    }

    fn load_literal(&mut self, lit: &Literal<'a>, layout: &Layout<'a>) -> Result<(), String> {
        let instruction = match lit {
            Literal::Bool(x) => I32Const(*x as i32),
            Literal::Byte(x) => I32Const(*x as i32),
            Literal::Int(x) => match layout {
                Layout::Builtin(Builtin::Int64) => I64Const(*x as i64),
                Layout::Builtin(
                    Builtin::Int32
                    | Builtin::Int16
                    | Builtin::Int8
                    | Builtin::Int1
                    | Builtin::Usize,
                ) => I32Const(*x as i32),
                x => {
                    return Err(format!("loading literal, {:?}, is not yet implemented", x));
                }
            },
            Literal::Float(x) => match layout {
                Layout::Builtin(Builtin::Float64) => F64Const((*x as f64).to_bits()),
                Layout::Builtin(Builtin::Float32) => F32Const((*x as f32).to_bits()),
                x => {
                    return Err(format!("loading literal, {:?}, is not yet implemented", x));
                }
            },
            x => {
                return Err(format!("loading literal, {:?}, is not yet implemented", x));
            }
        };
        self.code_builder.push(instruction);
        Ok(())
    }

    fn create_struct(
        &mut self,
        sym: &Symbol,
        layout: &Layout<'a>,
        fields: &'a [Symbol],
    ) -> Result<(), String> {
        // TODO: we just calculated storage and now we're getting it out of a map
        // Not passing it as an argument because I'm trying to match Backend method signatures
        let storage = self.get_symbol_storage(sym).to_owned();

        if let Layout::Struct(field_layouts) = layout {
            match storage {
                SymbolStorage::StackMemory { location, size, .. } => {
                    if size > 0 {
                        let (local_id, struct_offset) =
                            location.local_and_offset(self.stack_frame_pointer);
                        let mut field_offset = struct_offset;
                        for (field, _) in fields.iter().zip(field_layouts.iter()) {
                            field_offset +=
                                self.copy_symbol_to_memory(local_id, field_offset, *field);
                        }
                    } else {
                        return Err(format!("Not supported yet: zero-size struct at {:?}", sym));
                    }
                }
                _ => {
                    return Err(format!(
                        "Cannot create struct {:?} with storage {:?}",
                        sym, storage
                    ));
                }
            };
        } else {
            // Struct expression but not Struct layout => single element. Copy it.
            let field_storage = self.get_symbol_storage(&fields[0]).to_owned();
            self.copy_value_by_storage(&storage, &field_storage, fields[0]);
        }
        Ok(())
    }

    fn build_call_low_level(
        &mut self,
        lowlevel: &LowLevel,
        args: &'a [Symbol],
        return_layout: &Layout<'a>,
    ) -> Result<(), String> {
        self.load_symbols(args);
        let wasm_layout = WasmLayout::new(return_layout);
        self.build_instructions_lowlevel(lowlevel, wasm_layout.value_type())?;
        Ok(())
    }

    fn build_instructions_lowlevel(
        &mut self,
        lowlevel: &LowLevel,
        return_value_type: ValueType,
    ) -> Result<(), String> {
        // TODO:  Find a way to organise all the lowlevel ops and layouts! There's lots!
        //
        // Some Roc low-level ops care about wrapping, clipping, sign-extending...
        // For those, we'll need to pre-process each argument before the main op,
        // so simple arrays of instructions won't work. But there are common patterns.
        let instructions: &[Instruction] = match lowlevel {
            LowLevel::NumAdd => match return_value_type {
                ValueType::I32 => &[I32Add],
                ValueType::I64 => &[I64Add],
                ValueType::F32 => &[F32Add],
                ValueType::F64 => &[F64Add],
            },
            LowLevel::NumSub => match return_value_type {
                ValueType::I32 => &[I32Sub],
                ValueType::I64 => &[I64Sub],
                ValueType::F32 => &[F32Sub],
                ValueType::F64 => &[F64Sub],
            },
            LowLevel::NumMul => match return_value_type {
                ValueType::I32 => &[I32Mul],
                ValueType::I64 => &[I64Mul],
                ValueType::F32 => &[F32Mul],
                ValueType::F64 => &[F64Mul],
            },
            LowLevel::NumGt => {
                // needs layout of the argument to be implemented fully
                &[I32GtS]
            }
            _ => {
                return Err(format!("unsupported low-level op {:?}", lowlevel));
            }
        };
        self.code_builder.extend(instructions);
        Ok(())
    }
}
