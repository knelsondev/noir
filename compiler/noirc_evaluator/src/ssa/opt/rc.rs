use fxhash::{FxHashMap as HashMap, FxHashSet as HashSet};

use crate::ssa::{
    ir::{
        function::Function,
        instruction::{Instruction, InstructionId},
        types::Type,
        value::ValueId,
    },
    ssa_gen::Ssa,
};

impl Ssa {
    /// This pass removes `inc_rc` and `dec_rc` instructions
    /// as long as there are no `array_set` instructions to an array
    /// of the same type in between.
    ///
    /// Note that this pass is very conservative since the array_set
    /// instruction does not need to be to the same array. This is because
    /// the given array may alias another array (e.g. function parameters or
    /// a `load`ed array from a reference).
    #[tracing::instrument(level = "trace", skip(self))]
    pub(crate) fn remove_paired_rc(mut self) -> Ssa {
        for function in self.functions.values_mut() {
            function.remove_paired_rc();
        }
        self
    }
}

#[derive(Default)]
struct Context {
    // All inc_rc instructions encountered without a corresponding dec_rc.
    // These are only searched for in the first block of a function.
    //
    // The type of the array being operated on is recorded.
    // If an array_set to that array type is encountered, that is also recorded.
    inc_rcs: HashMap<Type, Vec<RcInstruction>>,
}

pub(crate) struct RcInstruction {
    pub(crate) id: InstructionId,
    pub(crate) array: ValueId,
    pub(crate) possibly_mutated: bool,
}

impl Function {
    /// This function is very simplistic for now. It takes advantage of the fact that dec_rc
    /// instructions are currently issued only at the end of a function for parameters and will
    /// only check the first and last block for inc & dec rc instructions to be removed. The rest
    /// of the function is still checked for array_set instructions.
    ///
    /// This restriction lets this function largely ignore merging intermediate results from other
    /// blocks and handling loops.
    pub(crate) fn remove_paired_rc(&mut self) {
        // `dec_rc` is only issued for parameters currently so we can speed things
        // up a bit by skipping any functions without them.
        if !contains_array_parameter(self) {
            return;
        }

        let mut context = Context::default();

        context.find_rcs_in_entry_block(self);
        context.scan_for_array_sets(self);
        let to_remove = context.find_rcs_to_remove(self);
        remove_instructions(to_remove, self);
    }
}

fn contains_array_parameter(function: &mut Function) -> bool {
    let mut parameters = function.parameters().iter();
    parameters.any(|parameter| function.dfg.type_of_value(*parameter).contains_an_array())
}

impl Context {
    fn find_rcs_in_entry_block(&mut self, function: &Function) {
        let entry = function.entry_block();

        for instruction in function.dfg[entry].instructions() {
            if let Instruction::IncrementRc { value } = &function.dfg[*instruction] {
                let typ = function.dfg.type_of_value(*value);

                // We assume arrays aren't mutated until we find an array_set
                let inc_rc =
                    RcInstruction { id: *instruction, array: *value, possibly_mutated: false };
                self.inc_rcs.entry(typ).or_default().push(inc_rc);
            }
        }
    }

    /// Find each array_set instruction in the function and mark any arrays used
    /// by the inc_rc instructions as possibly mutated if they're the same type.
    fn scan_for_array_sets(&mut self, function: &Function) {
        for block in function.reachable_blocks() {
            for instruction in function.dfg[block].instructions() {
                if let Instruction::ArraySet { array, .. } = function.dfg[*instruction] {
                    let typ = function.dfg.type_of_value(array);
                    if let Some(inc_rcs) = self.inc_rcs.get_mut(&typ) {
                        for inc_rc in inc_rcs {
                            inc_rc.possibly_mutated = true;
                        }
                    }
                }
            }
        }
    }

    /// Find each dec_rc instruction and if the most recent inc_rc instruction for the same value
    /// is not possibly mutated, then we can remove them both. Returns each such pair.
    fn find_rcs_to_remove(&mut self, function: &Function) -> HashSet<InstructionId> {
        let last_block = function.find_last_block();
        let mut to_remove = HashSet::default();

        for instruction in function.dfg[last_block].instructions() {
            if let Instruction::DecrementRc { value, .. } = &function.dfg[*instruction] {
                if let Some(inc_rc) = pop_rc_for(*value, function, &mut self.inc_rcs) {
                    if !inc_rc.possibly_mutated {
                        to_remove.insert(inc_rc.id);
                        to_remove.insert(*instruction);
                    }
                }
            }
        }

        to_remove
    }
}

/// Finds and pops the IncRc for the given array value if possible.
pub(crate) fn pop_rc_for(
    value: ValueId,
    function: &Function,
    inc_rcs: &mut HashMap<Type, Vec<RcInstruction>>,
) -> Option<RcInstruction> {
    let typ = function.dfg.type_of_value(value);

    let rcs = inc_rcs.get_mut(&typ)?;
    let position = rcs.iter().position(|inc_rc| inc_rc.array == value)?;

    Some(rcs.remove(position))
}

fn remove_instructions(to_remove: HashSet<InstructionId>, function: &mut Function) {
    if !to_remove.is_empty() {
        for block in function.reachable_blocks() {
            function.dfg[block]
                .instructions_mut()
                .retain(|instruction| !to_remove.contains(instruction));
        }
    }
}

#[cfg(test)]
mod test {

    use crate::ssa::{
        ir::{basic_block::BasicBlockId, dfg::DataFlowGraph, instruction::Instruction},
        ssa_gen::Ssa,
    };

    fn count_inc_rcs(block: BasicBlockId, dfg: &DataFlowGraph) -> usize {
        dfg[block]
            .instructions()
            .iter()
            .filter(|instruction_id| {
                matches!(dfg[**instruction_id], Instruction::IncrementRc { .. })
            })
            .count()
    }

    fn count_dec_rcs(block: BasicBlockId, dfg: &DataFlowGraph) -> usize {
        dfg[block]
            .instructions()
            .iter()
            .filter(|instruction_id| {
                matches!(dfg[**instruction_id], Instruction::DecrementRc { .. })
            })
            .count()
    }

    #[test]
    fn single_block_fn_return_array() {
        // This is the output for the program with a function:
        // unconstrained fn foo(x: [Field; 2]) -> [[Field; 2]; 1] {
        //     [array]
        // }
        let src = "
        brillig(inline) fn foo f0 {
          b0(v0: [Field; 2]):
            inc_rc v0
            inc_rc v0
            dec_rc v0
            v1 = make_array [v0] : [[Field; 2]; 1]
            return v1
        }
        ";
        let ssa = Ssa::from_str(src).unwrap();
        let ssa = ssa.remove_paired_rc();
        let main = ssa.main();
        let entry = main.entry_block();

        assert_eq!(count_inc_rcs(entry, &main.dfg), 1);
        assert_eq!(count_dec_rcs(entry, &main.dfg), 0);
    }

    #[test]
    fn single_block_mutation() {
        // fn mutator(mut array: [Field; 2]) {
        //     array[0] = 5;
        // }
        let src = "
        brillig(inline) fn mutator f0 {
          b0(v0: [Field; 2]):
            v1 = allocate -> &mut [Field; 2]
            store v0 at v1
            inc_rc v0
            v2 = load v1 -> [Field; 2]
            v5 = array_set v2, index u32 0, value Field 5
            store v5 at v1
            dec_rc v0
            return
        }
        ";

        let ssa = Ssa::from_str(src).unwrap();
        let ssa = ssa.remove_paired_rc();
        let main = ssa.main();
        let entry = main.entry_block();

        // No changes, the array is possibly mutated
        assert_eq!(count_inc_rcs(entry, &main.dfg), 1);
        assert_eq!(count_dec_rcs(entry, &main.dfg), 1);
    }

    // Similar to single_block_mutation but for a function which
    // uses a mutable reference parameter.
    #[test]
    fn single_block_mutation_through_reference() {
        // fn mutator2(array: &mut [Field; 2]) {
        //     array[0] = 5;
        // }
        let src = "
        brillig(inline) fn mutator2 f0 {
          b0(v0: &mut [Field; 2]):
            v1 = load v0 -> [Field; 2]
            inc_rc v1
            store v1 at v0
            v2 = load v1 -> [Field; 2]
            v5 = array_set v2, index u32 0, value Field 5
            store v5 at v0
            v6 = load v0 -> [Field; 2]
            dec_rc v1
            store v6 at v0
            return
        }
        ";

        let ssa = Ssa::from_str(src).unwrap();
        let ssa = ssa.remove_paired_rc();
        let main = ssa.main();
        let entry = main.entry_block();

        // No changes, the array is possibly mutated
        assert_eq!(count_inc_rcs(entry, &main.dfg), 1);
        assert_eq!(count_dec_rcs(entry, &main.dfg), 1);
    }
}
