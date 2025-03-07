//! ## Constraint system for LEM
//!
//! This module implements the generation of bellperson constraints for LEM, such
//! that it can be used with Nova folding to prove evaluations of Lurk expressions.
//!
//! ### "Concrete" and "virtual" paths
//!
//! Control statements such as matches introduce branching, meaning execution can
//! follow different paths depending on a value. The real path of execution for
//! a particular instance we call the *concrete path*. The other paths which are
//! not followed we call *virtual paths*. A mechanism to "relax" the constraints
//! for the virtual paths while also properly enforcing the correctness for what
//! happens on the concrete path is thus needed.
//!
//! We do that by using implication gadgets. An implication of the form `A → B`
//! is trivially true if `A` is false. But if `A` is true, then the implication
//! is true iff `B` is true. In other words, `B` is irrelevant if `A` is false,
//! which is the behavior we want for the virtual paths.
//!
//! With that in mind, we can keep track of booleans that tell us whether we're
//! on a concrete or a virtual path and use such booleans as the premises to build
//! the constraints we care about with implication gadgets.

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::{Context, Result};
use bellpepper_core::{
    ConstraintSystem, SynthesisError,
    {
        boolean::{AllocatedBit, Boolean},
        num::AllocatedNum,
    },
};

use crate::circuit::gadgets::{
    constraints::{
        add, alloc_equal, alloc_is_zero, allocate_is_negative, boolean_to_num, div, enforce_pack,
        enforce_product_and_sum, enforce_selector_with_premise, implies_equal, implies_equal_const,
        implies_u64, implies_unequal, implies_unequal_const, mul, pick, sub,
    },
    data::{allocate_constant, hash_poseidon},
    pointer::AllocatedPtr,
};

use crate::{
    field::{FWrap, LurkField},
    tag::ExprTag::*,
};

use super::{
    interpreter::{Frame, PreimageData},
    pointers::{Ptr, ZPtr},
    slot::*,
    store::Store,
    var_map::VarMap,
    Block, Ctrl, Func, Op, Tag, Var,
};

/// Manages global allocations for constants in a constraint system
#[derive(Default)]
pub(crate) struct GlobalAllocator<F: LurkField>(HashMap<FWrap<F>, AllocatedNum<F>>);

#[inline]
fn allocate_num<F: LurkField, CS: ConstraintSystem<F>>(
    cs: &mut CS,
    namespace: &str,
    value: F,
) -> Result<AllocatedNum<F>> {
    AllocatedNum::alloc(cs.namespace(|| namespace), || Ok(value))
        .with_context(|| format!("allocation for '{namespace}' failed"))
}

#[inline]
fn allocate_const<F: LurkField, CS: ConstraintSystem<F>>(
    cs: &mut CS,
    namespace: &str,
    value: F,
) -> Result<AllocatedNum<F>> {
    allocate_constant(&mut cs.namespace(|| namespace), value)
        .with_context(|| format!("allocation for '{namespace}' failed"))
}

impl<F: LurkField> GlobalAllocator<F> {
    /// Checks if the allocation for a numeric variable has already been cached.
    /// If so, return the cached allocation variable. Allocate as a constant,
    /// cache and return otherwise.
    pub(crate) fn get_or_alloc_const<CS: ConstraintSystem<F>>(
        &mut self,
        cs: &mut CS,
        f: F,
    ) -> Result<AllocatedNum<F>> {
        let wrap = FWrap(f);
        match self.0.get(&wrap) {
            Some(allocated_num) => Ok(allocated_num.to_owned()),
            None => {
                let allocated_num =
                    allocate_const(cs, &format!("allocate constant {}", f.hex_digits()), f)?;
                self.0.insert(wrap, allocated_num.clone());
                Ok(allocated_num)
            }
        }
    }
}

type BoundAllocations<F> = VarMap<AllocatedPtr<F>>;

impl Func {
    /// Allocates an unconstrained pointer
    fn allocate_ptr<F: LurkField, CS: ConstraintSystem<F>>(
        cs: &mut CS,
        z_ptr: &ZPtr<F>,
        var: &Var,
        bound_allocations: &mut BoundAllocations<F>,
    ) -> Result<AllocatedPtr<F>> {
        let allocated_tag =
            allocate_num(cs, &format!("allocate {var}'s tag"), z_ptr.tag.to_field())?;
        let allocated_hash = allocate_num(cs, &format!("allocate {var}'s hash"), z_ptr.hash)?;
        let allocated_ptr = AllocatedPtr::from_parts(allocated_tag, allocated_hash);
        bound_allocations.insert(var.clone(), allocated_ptr.clone());
        Ok(allocated_ptr)
    }

    /// Allocates an unconstrained pointer for each input of the frame
    fn allocate_input<F: LurkField, CS: ConstraintSystem<F>>(
        &self,
        cs: &mut CS,
        store: &mut Store<F>,
        frame: &Frame<F>,
        bound_allocations: &mut BoundAllocations<F>,
    ) -> Result<()> {
        for (i, ptr) in frame.input.iter().enumerate() {
            let param = &self.input_params[i];
            Self::allocate_ptr(cs, &store.hash_ptr(ptr)?, param, bound_allocations)?;
        }
        Ok(())
    }

    /// Allocates an unconstrained pointer for each output of the frame
    fn allocate_output<F: LurkField, CS: ConstraintSystem<F>>(
        cs: &mut CS,
        store: &mut Store<F>,
        frame: &Frame<F>,
        bound_allocations: &mut BoundAllocations<F>,
    ) -> Result<Vec<AllocatedPtr<F>>> {
        frame
            .output
            .iter()
            .enumerate()
            .map(|(i, ptr)| {
                Self::allocate_ptr(
                    cs,
                    &store.hash_ptr(ptr)?,
                    &Var(format!("output[{}]", i).into()),
                    bound_allocations,
                )
            })
            .collect::<Result<_>>()
    }

    #[inline]
    fn allocate_preimg_component_for_slot<F: LurkField, CS: ConstraintSystem<F>>(
        cs: &mut CS,
        slot: &Slot,
        component_idx: usize,
        value: F,
    ) -> Result<AllocatedNum<F>> {
        allocate_num(
            cs,
            &format!("component {component_idx} for slot {slot}"),
            value,
        )
    }

    fn allocate_img_for_slot<F: LurkField, CS: ConstraintSystem<F>>(
        cs: &mut CS,
        slot: &Slot,
        preallocated_preimg: Vec<AllocatedNum<F>>,
        store: &mut Store<F>,
    ) -> Result<AllocatedNum<F>> {
        let cs = &mut cs.namespace(|| format!("image for slot {slot}"));
        let preallocated_img = {
            match slot.typ {
                SlotType::Hash2 => {
                    hash_poseidon(cs, preallocated_preimg, store.poseidon_cache.constants.c4())?
                }
                SlotType::Hash3 => {
                    hash_poseidon(cs, preallocated_preimg, store.poseidon_cache.constants.c6())?
                }
                SlotType::Hash4 => {
                    hash_poseidon(cs, preallocated_preimg, store.poseidon_cache.constants.c8())?
                }
                SlotType::Commitment => {
                    hash_poseidon(cs, preallocated_preimg, store.poseidon_cache.constants.c3())?
                }
                SlotType::LessThan => {
                    let a_num = &preallocated_preimg[0];
                    let b_num = &preallocated_preimg[1];
                    let diff = sub(
                        &mut cs.namespace(|| format!("sub for slot {slot}")),
                        a_num,
                        b_num,
                    )?;
                    let diff_is_negative = allocate_is_negative(
                        &mut cs.namespace(|| format!("is_negative for slot {slot}")),
                        &diff,
                    )?;
                    boolean_to_num(
                        &mut cs.namespace(|| format!("boolean_to_num for slot {slot}")),
                        &diff_is_negative,
                    )?
                }
            }
        };
        Ok(preallocated_img)
    }

    /// Allocates unconstrained slots
    fn allocate_slots<F: LurkField, CS: ConstraintSystem<F>>(
        cs: &mut CS,
        preimg_data: &[Option<PreimageData<F>>],
        slot_type: SlotType,
        num_slots: usize,
        store: &mut Store<F>,
    ) -> Result<Vec<(Vec<AllocatedNum<F>>, AllocatedNum<F>)>> {
        assert!(
            preimg_data.len() == num_slots,
            "collected preimages not equal to the number of available slots"
        );

        let mut preallocations = Vec::with_capacity(num_slots);

        // We must perform the allocations for the slots containing data collected
        // by the interpreter. The `None` cases must be filled with dummy values
        for (slot_idx, maybe_preimg_data) in preimg_data.iter().enumerate() {
            if let Some(preimg_data) = maybe_preimg_data {
                let slot = Slot {
                    idx: slot_idx,
                    typ: slot_type,
                };

                // Allocate the preimage because the image depends on it
                let mut preallocated_preimg = Vec::with_capacity(slot_type.preimg_size());

                match preimg_data {
                    PreimageData::PtrVec(ptr_vec) => {
                        let mut component_idx = 0;
                        for ptr in ptr_vec {
                            let z_ptr = store.hash_ptr(ptr)?;

                            // allocate pointer tag
                            preallocated_preimg.push(Self::allocate_preimg_component_for_slot(
                                cs,
                                &slot,
                                component_idx,
                                z_ptr.tag.to_field(),
                            )?);

                            component_idx += 1;

                            // allocate pointer hash
                            preallocated_preimg.push(Self::allocate_preimg_component_for_slot(
                                cs,
                                &slot,
                                component_idx,
                                z_ptr.hash,
                            )?);

                            component_idx += 1;
                        }
                    }
                    PreimageData::FPtr(f, ptr) => {
                        let z_ptr = store.hash_ptr(ptr)?;
                        // allocate first component
                        preallocated_preimg
                            .push(Self::allocate_preimg_component_for_slot(cs, &slot, 0, *f)?);
                        // allocate second component
                        preallocated_preimg.push(Self::allocate_preimg_component_for_slot(
                            cs,
                            &slot,
                            1,
                            z_ptr.tag.to_field(),
                        )?);
                        // allocate third component
                        preallocated_preimg.push(Self::allocate_preimg_component_for_slot(
                            cs, &slot, 2, z_ptr.hash,
                        )?);
                    }
                    PreimageData::FPair(a, b) => {
                        // allocate first component
                        preallocated_preimg
                            .push(Self::allocate_preimg_component_for_slot(cs, &slot, 0, *a)?);

                        // allocate second component
                        preallocated_preimg
                            .push(Self::allocate_preimg_component_for_slot(cs, &slot, 1, *b)?);
                    }
                }

                // Allocate the image by calling the arithmetic function according
                // to the slot type
                let preallocated_img =
                    Self::allocate_img_for_slot(cs, &slot, preallocated_preimg.clone(), store)?;

                preallocations.push((preallocated_preimg, preallocated_img));
            } else {
                let slot = Slot {
                    idx: slot_idx,
                    typ: slot_type,
                };
                let preallocated_preimg: Vec<_> = (0..slot_type.preimg_size())
                    .map(|component_idx| {
                        Self::allocate_preimg_component_for_slot(cs, &slot, component_idx, F::ZERO)
                    })
                    .collect::<Result<_, _>>()?;

                let preallocated_img =
                    Self::allocate_img_for_slot(cs, &slot, preallocated_preimg.clone(), store)?;

                preallocations.push((preallocated_preimg, preallocated_img));
            }
        }

        Ok(preallocations)
    }

    /// Create R1CS constraints for a LEM function given an evaluation frame. This
    /// function implements the STEP 3 mentioned above.
    ///
    /// Regarding the slot optimizations, STEP 3 uses information gathered during
    /// STEPs 1 and 2. So we proceed by first allocating preimages and images for
    /// each slot and then, as we traverse the function, we add constraints to make
    /// sure that the witness satisfies the arithmetic equations for the
    /// corresponding slots.
    pub fn synthesize<F: LurkField, CS: ConstraintSystem<F>>(
        &self,
        cs: &mut CS,
        store: &mut Store<F>,
        frame: &Frame<F>,
    ) -> Result<()> {
        let mut global_allocator = GlobalAllocator::default();
        let mut bound_allocations = BoundAllocations::new();

        // Inputs are constrained by their usage inside the function body
        self.allocate_input(cs, store, frame, &mut bound_allocations)?;
        // Outputs are constrained by the return statement. All functions return
        let preallocated_outputs = Func::allocate_output(cs, store, frame, &mut bound_allocations)?;

        // Slots are constrained by their usage inside the function body. The ones
        // not used in throughout the concrete path are effectively unconstrained,
        // that's why they are filled with dummies
        let preallocated_hash2_slots = Func::allocate_slots(
            cs,
            &frame.preimages.hash2,
            SlotType::Hash2,
            self.slot.hash2,
            store,
        )?;

        let preallocated_hash3_slots = Func::allocate_slots(
            cs,
            &frame.preimages.hash3,
            SlotType::Hash3,
            self.slot.hash3,
            store,
        )?;

        let preallocated_hash4_slots = Func::allocate_slots(
            cs,
            &frame.preimages.hash4,
            SlotType::Hash4,
            self.slot.hash4,
            store,
        )?;

        let preallocated_commitment_slots = Func::allocate_slots(
            cs,
            &frame.preimages.commitment,
            SlotType::Commitment,
            self.slot.commitment,
            store,
        )?;

        let preallocated_less_than_slots = Func::allocate_slots(
            cs,
            &frame.preimages.less_than,
            SlotType::LessThan,
            self.slot.less_than,
            store,
        )?;

        struct Globals<'a, F: LurkField> {
            store: &'a mut Store<F>,
            global_allocator: &'a mut GlobalAllocator<F>,
            preallocated_hash2_slots: Vec<(Vec<AllocatedNum<F>>, AllocatedNum<F>)>,
            preallocated_hash3_slots: Vec<(Vec<AllocatedNum<F>>, AllocatedNum<F>)>,
            preallocated_hash4_slots: Vec<(Vec<AllocatedNum<F>>, AllocatedNum<F>)>,
            preallocated_commitment_slots: Vec<(Vec<AllocatedNum<F>>, AllocatedNum<F>)>,
            preallocated_less_than_slots: Vec<(Vec<AllocatedNum<F>>, AllocatedNum<F>)>,
            call_outputs: VecDeque<Vec<Ptr<F>>>,
            call_count: usize,
        }

        fn recurse<F: LurkField, CS: ConstraintSystem<F>>(
            cs: &mut CS,
            block: &Block,
            not_dummy: &Boolean,
            next_slot: &mut SlotsCounter,
            bound_allocations: &mut BoundAllocations<F>,
            preallocated_outputs: &Vec<AllocatedPtr<F>>,
            g: &mut Globals<'_, F>,
        ) -> Result<()> {
            for op in &block.ops {
                macro_rules! hash_helper {
                    ( $img: expr, $tag: expr, $preimg: expr, $slot: expr ) => {
                        // Retrieve allocated preimage
                        let allocated_preimg = bound_allocations.get_many($preimg)?;

                        // Retrieve the preallocated preimage and image for this slot
                        let (preallocated_preimg, preallocated_img_hash) = match $slot {
                            SlotType::Hash2 => {
                                &g.preallocated_hash2_slots[next_slot.consume_hash2()]
                            }
                            SlotType::Hash3 => {
                                &g.preallocated_hash3_slots[next_slot.consume_hash3()]
                            }
                            SlotType::Hash4 => {
                                &g.preallocated_hash4_slots[next_slot.consume_hash4()]
                            }
                            _ => panic!("Invalid slot type for hash_helper macro"),
                        };

                        // For each component of the preimage, add implication constraints
                        // for its tag and hash
                        for (i, allocated_ptr) in allocated_preimg.iter().enumerate() {
                            let var = &$preimg[i];
                            let ptr_idx = 2 * i;
                            implies_equal(
                                &mut cs.namespace(|| {
                                    format!("implies equal for {var}'s tag (OP {:?}, pos {i})", &op)
                                }),
                                not_dummy,
                                allocated_ptr.tag(),
                                &preallocated_preimg[ptr_idx], // tag index
                            )?;
                            implies_equal(
                                &mut cs.namespace(|| {
                                    format!(
                                        "implies equal for {var}'s hash (OP {:?}, pos {i})",
                                        &op
                                    )
                                }),
                                not_dummy,
                                allocated_ptr.hash(),
                                &preallocated_preimg[ptr_idx + 1], // hash index
                            )?;
                        }

                        // Allocate the image tag if it hasn't been allocated before,
                        // create the full image pointer and add it to bound allocations
                        let img_tag = g.global_allocator.get_or_alloc_const(cs, $tag.to_field())?;
                        let img_hash = preallocated_img_hash.clone();
                        let img_ptr = AllocatedPtr::from_parts(img_tag, img_hash);
                        bound_allocations.insert($img, img_ptr);
                    };
                }

                macro_rules! unhash_helper {
                    ( $preimg: expr, $img: expr, $slot: expr ) => {
                        // Retrieve allocated image
                        let allocated_img = bound_allocations.get($img)?;

                        // Retrieve the preallocated preimage and image for this slot
                        let (preallocated_preimg, preallocated_img) = match $slot {
                            SlotType::Hash2 => {
                                &g.preallocated_hash2_slots[next_slot.consume_hash2()]
                            }
                            SlotType::Hash3 => {
                                &g.preallocated_hash3_slots[next_slot.consume_hash3()]
                            }
                            SlotType::Hash4 => {
                                &g.preallocated_hash4_slots[next_slot.consume_hash4()]
                            }
                            _ => panic!("Invalid slot type for unhash_helper macro"),
                        };

                        // Add the implication constraint for the image
                        implies_equal(
                            &mut cs.namespace(|| {
                                format!("implies equal for {}'s hash (OP {:?})", $img, &op)
                            }),
                            not_dummy,
                            allocated_img.hash(),
                            &preallocated_img,
                        )?;

                        // Retrieve preimage hashes and tags create the full preimage pointers
                        // and add them to bound allocations
                        for i in 0..preallocated_preimg.len() / 2 {
                            let preimg_tag = &preallocated_preimg[2 * i];
                            let preimg_hash = &preallocated_preimg[2 * i + 1];
                            let preimg_ptr =
                                AllocatedPtr::from_parts(preimg_tag.clone(), preimg_hash.clone());
                            bound_allocations.insert($preimg[i].clone(), preimg_ptr);
                        }
                    };
                }

                match op {
                    Op::Call(out, func, inp) => {
                        // Allocate the output pointers that the `func` will return to.
                        // These should be unconstrained as of yet, and will be constrained
                        // by the return statements inside `func`.
                        // Note that, because there's currently no way of deferring giving
                        // a value to the allocated nums to be filled later, we must either
                        // add the results of the call to the witness, or recompute them.
                        let output_vals = if let Some(true) = not_dummy.get_value() {
                            g.call_outputs.pop_front().unwrap()
                        } else {
                            let dummy = Ptr::Leaf(Tag::Expr(Nil), F::ZERO);
                            (0..out.len()).map(|_| dummy).collect()
                        };
                        assert_eq!(output_vals.len(), out.len());
                        let mut output_ptrs = Vec::with_capacity(out.len());
                        for (ptr, var) in output_vals.iter().zip(out.iter()) {
                            let zptr = &g.store.hash_ptr(ptr)?;
                            output_ptrs.push(Func::allocate_ptr(cs, zptr, var, bound_allocations)?);
                        }
                        // Get the pointers for the input, i.e. the arguments
                        let args = bound_allocations.get_many_cloned(inp)?;
                        // These are the input parameters (formal variables)
                        let param_list = func.input_params.iter();
                        // Now we bind the `Func`'s input parameters to the arguments in the call.
                        param_list.zip(args.into_iter()).for_each(|(param, arg)| {
                            bound_allocations.insert(param.clone(), arg);
                        });
                        // Finally, we synthesize the circuit for the function body
                        g.call_count += 1;
                        recurse(
                            &mut cs.namespace(|| format!("Call {}", g.call_count)),
                            &func.body,
                            not_dummy,
                            next_slot,
                            bound_allocations,
                            &output_ptrs,
                            g,
                        )?;
                    }
                    Op::Hash2(img, tag, preimg) => {
                        hash_helper!(img.clone(), tag, preimg, SlotType::Hash2);
                    }
                    Op::Hash3(img, tag, preimg) => {
                        hash_helper!(img.clone(), tag, preimg, SlotType::Hash3);
                    }
                    Op::Hash4(img, tag, preimg) => {
                        hash_helper!(img.clone(), tag, preimg, SlotType::Hash4);
                    }
                    Op::Unhash2(preimg, img) => {
                        unhash_helper!(preimg, img, SlotType::Hash2);
                    }
                    Op::Unhash3(preimg, img) => {
                        unhash_helper!(preimg, img, SlotType::Hash3);
                    }
                    Op::Unhash4(preimg, img) => {
                        unhash_helper!(preimg, img, SlotType::Hash4);
                    }
                    Op::Null(tgt, tag) => {
                        let tag = g.global_allocator.get_or_alloc_const(cs, tag.to_field())?;
                        let zero = g.global_allocator.get_or_alloc_const(cs, F::ZERO)?;
                        let allocated_ptr = AllocatedPtr::from_parts(tag, zero);
                        bound_allocations.insert(tgt.clone(), allocated_ptr);
                    }
                    Op::Lit(tgt, lit) => {
                        let lit_ptr = lit.to_ptr(g.store);
                        let lit_tag = lit_ptr.tag().to_field();
                        let lit_hash = g.store.hash_ptr(&lit_ptr)?.hash;
                        let allocated_tag = g.global_allocator.get_or_alloc_const(cs, lit_tag)?;
                        let allocated_hash = g.global_allocator.get_or_alloc_const(cs, lit_hash)?;
                        let allocated_ptr = AllocatedPtr::from_parts(allocated_tag, allocated_hash);
                        bound_allocations.insert(tgt.clone(), allocated_ptr);
                    }
                    Op::Cast(tgt, tag, src) => {
                        let src = bound_allocations.get(src)?;
                        let tag = g.global_allocator.get_or_alloc_const(cs, tag.to_field())?;
                        let allocated_ptr = AllocatedPtr::from_parts(tag, src.hash().clone());
                        bound_allocations.insert(tgt.clone(), allocated_ptr);
                    }
                    Op::EqTag(tgt, a, b) => {
                        let a = bound_allocations.get(a)?;
                        let b = bound_allocations.get(b)?;
                        let a_num = a.tag();
                        let b_num = b.tag();
                        let eq = alloc_equal(&mut cs.namespace(|| "equal_tag"), a_num, b_num)?;
                        let c_num = boolean_to_num(&mut cs.namespace(|| "equal_tag.to_num"), &eq)?;
                        let tag = g
                            .global_allocator
                            .get_or_alloc_const(cs, Tag::Expr(Num).to_field())?;
                        let c = AllocatedPtr::from_parts(tag, c_num);
                        bound_allocations.insert(tgt.clone(), c);
                    }
                    Op::EqVal(tgt, a, b) => {
                        let a = bound_allocations.get(a)?;
                        let b = bound_allocations.get(b)?;
                        let a_num = a.hash();
                        let b_num = b.hash();
                        let eq = alloc_equal(&mut cs.namespace(|| "equal_val"), a_num, b_num)?;
                        let c_num = boolean_to_num(&mut cs.namespace(|| "equal_val.to_num"), &eq)?;
                        let tag = g
                            .global_allocator
                            .get_or_alloc_const(cs, Tag::Expr(Num).to_field())?;
                        let c = AllocatedPtr::from_parts(tag, c_num);
                        bound_allocations.insert(tgt.clone(), c);
                    }
                    Op::Add(tgt, a, b) => {
                        let a = bound_allocations.get(a)?;
                        let b = bound_allocations.get(b)?;
                        let a_num = a.hash();
                        let b_num = b.hash();
                        let c_num = add(&mut cs.namespace(|| "add"), a_num, b_num)?;
                        let tag = g
                            .global_allocator
                            .get_or_alloc_const(cs, Tag::Expr(Num).to_field())?;
                        let c = AllocatedPtr::from_parts(tag, c_num);
                        bound_allocations.insert(tgt.clone(), c);
                    }
                    Op::Sub(tgt, a, b) => {
                        let a = bound_allocations.get(a)?;
                        let b = bound_allocations.get(b)?;
                        let a_num = a.hash();
                        let b_num = b.hash();
                        let c_num = sub(&mut cs.namespace(|| "sub"), a_num, b_num)?;
                        let tag = g
                            .global_allocator
                            .get_or_alloc_const(cs, Tag::Expr(Num).to_field())?;
                        let c = AllocatedPtr::from_parts(tag, c_num);
                        bound_allocations.insert(tgt.clone(), c);
                    }
                    Op::Mul(tgt, a, b) => {
                        let a = bound_allocations.get(a)?;
                        let b = bound_allocations.get(b)?;
                        let a_num = a.hash();
                        let b_num = b.hash();
                        let c_num = mul(&mut cs.namespace(|| "mul"), a_num, b_num)?;
                        let tag = g
                            .global_allocator
                            .get_or_alloc_const(cs, Tag::Expr(Num).to_field())?;
                        let c = AllocatedPtr::from_parts(tag, c_num);
                        bound_allocations.insert(tgt.clone(), c);
                    }
                    Op::Div(tgt, a, b) => {
                        let a = bound_allocations.get(a)?;
                        let b = bound_allocations.get(b)?;
                        let a_num = a.hash();
                        let b_num = b.hash();

                        let b_is_zero = &alloc_is_zero(&mut cs.namespace(|| "b_is_zero"), b_num)?;
                        let one = g.global_allocator.get_or_alloc_const(cs, F::ONE)?;

                        let divisor = pick(
                            &mut cs.namespace(|| "maybe-dummy divisor"),
                            b_is_zero,
                            &one,
                            b_num,
                        )?;

                        let quotient = div(&mut cs.namespace(|| "quotient"), a_num, &divisor)?;

                        let tag = g
                            .global_allocator
                            .get_or_alloc_const(cs, Tag::Expr(Num).to_field())?;
                        let c = AllocatedPtr::from_parts(tag, quotient);
                        bound_allocations.insert(tgt.clone(), c);
                    }
                    Op::Lt(tgt, a, b) => {
                        let a = bound_allocations.get(a)?;
                        let b = bound_allocations.get(b)?;
                        let tag = g
                            .global_allocator
                            .get_or_alloc_const(cs, Tag::Expr(Num).to_field())?;
                        let (preallocated_preimg, lt) =
                            &g.preallocated_less_than_slots[next_slot.consume_less_than()];
                        for (i, n) in [a.hash(), b.hash()].into_iter().enumerate() {
                            implies_equal(
                                &mut cs.namespace(|| {
                                    format!("implies equal for component {i} (OP {:?})", &op)
                                }),
                                not_dummy,
                                n,
                                &preallocated_preimg[i],
                            )?;
                        }
                        let c = AllocatedPtr::from_parts(tag, lt.clone());
                        bound_allocations.insert(tgt.clone(), c);
                    }
                    Op::Trunc(tgt, a, n) => {
                        assert!(*n <= 64);
                        let a = bound_allocations.get(a)?;
                        let mut trunc_bits = a
                            .hash()
                            .to_bits_le_strict(&mut cs.namespace(|| "to_bits_le"))?;
                        trunc_bits.truncate(*n as usize);
                        let trunc = AllocatedNum::alloc(cs.namespace(|| "trunc"), || {
                            let b = if *n < 64 { (1 << *n) - 1 } else { u64::MAX };
                            a.hash()
                                .get_value()
                                .map(|a| F::from_u64(a.to_u64_unchecked() & b))
                                .ok_or(SynthesisError::AssignmentMissing)
                        })?;
                        enforce_pack(&mut cs.namespace(|| "enforce_trunc"), &trunc_bits, &trunc)?;
                        let tag = g
                            .global_allocator
                            .get_or_alloc_const(cs, Tag::Expr(Num).to_field())?;
                        let c = AllocatedPtr::from_parts(tag, trunc);
                        bound_allocations.insert(tgt.clone(), c);
                    }
                    Op::DivRem64(tgt, a, b) => {
                        let a = bound_allocations.get(a)?.hash();
                        let b = bound_allocations.get(b)?.hash();
                        let div_rem = a.get_value().and_then(|a| {
                            b.get_value().map(|b| {
                                if not_dummy.get_value().unwrap() {
                                    let a = a.to_u64_unchecked();
                                    let b = b.to_u64_unchecked();
                                    (F::from_u64(a / b), F::from_u64(a % b))
                                } else {
                                    (F::ZERO, a)
                                }
                            })
                        });
                        let div =
                            AllocatedNum::alloc(cs.namespace(|| "div"), || Ok(div_rem.unwrap().0))?;
                        let rem =
                            AllocatedNum::alloc(cs.namespace(|| "rem"), || Ok(div_rem.unwrap().1))?;

                        let diff = sub(cs.namespace(|| "diff for slot {slot}"), b, &rem)?;
                        implies_u64(cs.namespace(|| "div_u64"), not_dummy, &div)?;
                        implies_u64(cs.namespace(|| "rem_u64"), not_dummy, &rem)?;
                        implies_u64(cs.namespace(|| "diff_u64"), not_dummy, &diff)?;

                        enforce_product_and_sum(
                            cs,
                            || "enforce a = b * div + rem",
                            b,
                            &div,
                            &rem,
                            a,
                        );
                        let tag = g
                            .global_allocator
                            .get_or_alloc_const(cs, Tag::Expr(Num).to_field())?;
                        let div_ptr = AllocatedPtr::from_parts(tag.clone(), div);
                        let rem_ptr = AllocatedPtr::from_parts(tag, rem);
                        bound_allocations.insert(tgt[0].clone(), div_ptr);
                        bound_allocations.insert(tgt[1].clone(), rem_ptr);
                    }
                    Op::Emit(_) => (),
                    Op::Hide(tgt, sec, pay) => {
                        let sec = bound_allocations.get(sec)?;
                        let pay = bound_allocations.get(pay)?;
                        let sec_tag = g
                            .global_allocator
                            .get_or_alloc_const(cs, Tag::Expr(Num).to_field())?;
                        let (preallocated_preimg, hash) =
                            &g.preallocated_commitment_slots[next_slot.consume_commitment()];
                        implies_equal(
                            &mut cs.namespace(|| {
                                format!("implies equal for the secret's tag (OP {:?})", &op)
                            }),
                            not_dummy,
                            sec.tag(),
                            &sec_tag,
                        )?;
                        implies_equal(
                            &mut cs.namespace(|| {
                                format!("implies equal for the secret's hash (OP {:?})", &op)
                            }),
                            not_dummy,
                            sec.hash(),
                            &preallocated_preimg[0],
                        )?;
                        implies_equal(
                            &mut cs.namespace(|| {
                                format!("implies equal for the payload's tag (OP {:?})", &op)
                            }),
                            not_dummy,
                            pay.tag(),
                            &preallocated_preimg[1],
                        )?;
                        implies_equal(
                            &mut cs.namespace(|| {
                                format!("implies equal for the payload's hash (OP {:?})", &op)
                            }),
                            not_dummy,
                            pay.hash(),
                            &preallocated_preimg[2],
                        )?;
                        let tag = g
                            .global_allocator
                            .get_or_alloc_const(cs, Tag::Expr(Comm).to_field())?;
                        let allocated_ptr = AllocatedPtr::from_parts(tag, hash.clone());
                        bound_allocations.insert(tgt.clone(), allocated_ptr);
                    }
                    Op::Open(sec, pay, comm) => {
                        let comm = bound_allocations.get(comm)?;
                        let (preallocated_preimg, com_hash) =
                            &g.preallocated_commitment_slots[next_slot.consume_commitment()];
                        let comm_tag = g
                            .global_allocator
                            .get_or_alloc_const(cs, Tag::Expr(Comm).to_field())?;
                        implies_equal(
                            &mut cs.namespace(|| {
                                format!("implies equal for comm's tag (OP {:?})", &op)
                            }),
                            not_dummy,
                            comm.tag(),
                            &comm_tag,
                        )?;
                        implies_equal(
                            &mut cs.namespace(|| {
                                format!("implies equal for comm's hash (OP {:?})", &op)
                            }),
                            not_dummy,
                            comm.hash(),
                            com_hash,
                        )?;
                        let sec_tag = g
                            .global_allocator
                            .get_or_alloc_const(cs, Tag::Expr(Num).to_field())?;
                        let allocated_sec_ptr =
                            AllocatedPtr::from_parts(sec_tag, preallocated_preimg[0].clone());
                        let allocated_pay_ptr = AllocatedPtr::from_parts(
                            preallocated_preimg[1].clone(),
                            preallocated_preimg[2].clone(),
                        );
                        bound_allocations.insert(sec.clone(), allocated_sec_ptr);
                        bound_allocations.insert(pay.clone(), allocated_pay_ptr);
                    }
                }
            }

            match &block.ctrl {
                Ctrl::Return(return_vars) => {
                    for (i, return_var) in return_vars.iter().enumerate() {
                        let allocated_ptr = bound_allocations.get(return_var)?;

                        allocated_ptr
                            .implies_ptr_equal(
                                &mut cs.namespace(|| {
                                    format!("implies_ptr_equal {return_var} (return_var {i})")
                                }),
                                not_dummy,
                                &preallocated_outputs[i],
                            )
                            .with_context(|| "couldn't constrain `implies_ptr_equal`")?;
                    }
                    Ok(())
                }
                Ctrl::IfEq(x, y, eq_block, else_block) => {
                    let x_ptr = bound_allocations.get(x)?.hash();
                    let y_ptr = bound_allocations.get(y)?.hash();
                    let mut selector = Vec::with_capacity(3);

                    let eq_val = not_dummy.get_value().and_then(|not_dummy| {
                        x_ptr
                            .get_value()
                            .and_then(|x| y_ptr.get_value().map(|y| not_dummy && x == y))
                    });
                    let neq_val = not_dummy.get_value().and_then(|not_dummy| {
                        x_ptr
                            .get_value()
                            .and_then(|x| y_ptr.get_value().map(|y| not_dummy && x != y))
                    });
                    let is_eq =
                        Boolean::Is(AllocatedBit::alloc(&mut cs.namespace(|| "if_eq"), eq_val)?);
                    let is_neq = Boolean::Is(AllocatedBit::alloc(
                        &mut cs.namespace(|| "if_neq"),
                        neq_val,
                    )?);
                    implies_equal(
                        &mut cs.namespace(|| format!("{x} = {y}")),
                        &is_eq,
                        x_ptr,
                        y_ptr,
                    )?;
                    implies_unequal(
                        &mut cs.namespace(|| format!("{x} != {y}")),
                        &is_neq,
                        x_ptr,
                        y_ptr,
                    )?;

                    selector.push(not_dummy.not());
                    selector.push(is_eq.clone());
                    selector.push(is_neq.clone());
                    enforce_selector_with_premise(
                        &mut cs.namespace(|| "if_enforce_selector_with_premise"),
                        not_dummy,
                        &selector,
                    )
                    .with_context(|| " couldn't constrain `enforce_selector_with_premise`")?;

                    let mut branch_slot = *next_slot;
                    recurse(
                        &mut cs.namespace(|| "if_eq.true"),
                        eq_block,
                        &is_eq,
                        &mut branch_slot,
                        bound_allocations,
                        preallocated_outputs,
                        g,
                    )?;
                    recurse(
                        &mut cs.namespace(|| "if_eq.false"),
                        else_block,
                        &is_neq,
                        next_slot,
                        bound_allocations,
                        preallocated_outputs,
                        g,
                    )?;
                    *next_slot = next_slot.max(branch_slot);
                    Ok(())
                }
                Ctrl::MatchTag(match_var, cases, def) => {
                    let match_tag = bound_allocations.get(match_var)?.tag().clone();
                    let mut selector = Vec::with_capacity(cases.len() + 2);
                    let mut branch_slots = Vec::with_capacity(cases.len());
                    for (tag, block) in cases {
                        let is_eq = not_dummy.get_value().and_then(|not_dummy| {
                            match_tag
                                .get_value()
                                .map(|val| not_dummy && val == tag.to_field::<F>())
                        });

                        let has_match = Boolean::Is(AllocatedBit::alloc(
                            &mut cs.namespace(|| format!("{tag}.allocated_bit")),
                            is_eq,
                        )?);
                        implies_equal_const(
                            &mut cs.namespace(|| format!("implies equal for {match_var}'s {tag}")),
                            &has_match,
                            &match_tag,
                            tag.to_field(),
                        )?;

                        selector.push(has_match.clone());

                        let mut branch_slot = *next_slot;
                        recurse(
                            &mut cs.namespace(|| format!("{}", tag)),
                            block,
                            &has_match,
                            &mut branch_slot,
                            bound_allocations,
                            preallocated_outputs,
                            g,
                        )?;
                        branch_slots.push(branch_slot);
                    }

                    match def {
                        Some(def) => {
                            let default = selector.iter().fold(not_dummy.get_value(), |acc, b| {
                                acc.and_then(|acc| b.get_value().map(|b| acc && !b))
                            });
                            let has_match = Boolean::Is(AllocatedBit::alloc(
                                &mut cs.namespace(|| "_.allocated_bit"),
                                default,
                            )?);
                            for (tag, _) in cases {
                                implies_unequal_const(
                                    &mut cs.namespace(|| format!("{tag} implies_unequal")),
                                    &has_match,
                                    &match_tag,
                                    tag.to_field(),
                                )?;
                            }

                            selector.push(has_match.clone());

                            recurse(
                                &mut cs.namespace(|| "_"),
                                def,
                                &has_match,
                                next_slot,
                                bound_allocations,
                                preallocated_outputs,
                                g,
                            )?;
                        }
                        None => (),
                    }

                    // The number of slots the match used is the max number of slots of each branch
                    *next_slot = branch_slots
                        .into_iter()
                        .fold(*next_slot, |acc, branch_slot| acc.max(branch_slot));

                    // Now we need to enforce that at exactly one path was taken. We do that by enforcing
                    // that the sum of the previously collected `Boolean`s is one. But, of course, this
                    // irrelevant if we're on a virtual path and thus we use an implication gadget.
                    selector.push(not_dummy.not());
                    enforce_selector_with_premise(
                        &mut cs.namespace(|| "enforce_selector_with_premise"),
                        not_dummy,
                        &selector,
                    )
                    .with_context(|| " couldn't constrain `enforce_selector_with_premise`")
                }
                Ctrl::MatchVal(match_var, cases, def) => {
                    let match_lit = bound_allocations.get(match_var)?.hash().clone();
                    let mut selector = Vec::with_capacity(cases.len() + 2);
                    let mut branch_slots = Vec::with_capacity(cases.len());
                    for (i, (lit, block)) in cases.iter().enumerate() {
                        let lit_ptr = lit.to_ptr(g.store);
                        let lit_hash = g.store.hash_ptr(&lit_ptr)?.hash;
                        let is_eq = not_dummy.get_value().and_then(|not_dummy| {
                            match_lit
                                .get_value()
                                .map(|val| not_dummy && val == lit_hash)
                        });

                        let has_match = Boolean::Is(AllocatedBit::alloc(
                            &mut cs.namespace(|| format!("{i}.allocated_bit")),
                            is_eq,
                        )?);
                        implies_equal_const(
                            &mut cs.namespace(|| format!("implies equal for {match_var} ({i})")),
                            &has_match,
                            &match_lit,
                            lit_hash,
                        )?;

                        selector.push(has_match.clone());

                        let mut branch_slot = *next_slot;
                        recurse(
                            &mut cs.namespace(|| format!("{i}.case")),
                            block,
                            &has_match,
                            &mut branch_slot,
                            bound_allocations,
                            preallocated_outputs,
                            g,
                        )?;
                        branch_slots.push(branch_slot);
                    }

                    match def {
                        Some(def) => {
                            let default = selector.iter().fold(not_dummy.get_value(), |acc, b| {
                                acc.and_then(|acc| b.get_value().map(|b| acc && !b))
                            });
                            let has_match = Boolean::Is(AllocatedBit::alloc(
                                &mut cs.namespace(|| "_.allocated_bit"),
                                default,
                            )?);
                            for (i, (lit, _)) in cases.iter().enumerate() {
                                let lit_ptr = lit.to_ptr(g.store);
                                let lit_hash = g.store.hash_ptr(&lit_ptr)?.hash;
                                implies_unequal_const(
                                    &mut cs.namespace(|| format!("{i} implies_unequal")),
                                    &has_match,
                                    &match_lit,
                                    lit_hash,
                                )?;
                            }

                            selector.push(has_match.clone());

                            recurse(
                                &mut cs.namespace(|| "_"),
                                def,
                                &has_match,
                                next_slot,
                                bound_allocations,
                                preallocated_outputs,
                                g,
                            )?;
                        }
                        None => (),
                    }

                    // The number of slots the match used is the max number of slots of each branch
                    *next_slot = branch_slots
                        .into_iter()
                        .fold(*next_slot, |acc, branch_slot| acc.max(branch_slot));

                    // Now we need to enforce that at exactly one path was taken. We do that by enforcing
                    // that the sum of the previously collected `Boolean`s is one. But, of course, this
                    // irrelevant if we're on a virtual path and thus we use an implication gadget.
                    selector.push(not_dummy.not());
                    enforce_selector_with_premise(
                        &mut cs.namespace(|| "enforce_selector_with_premise"),
                        not_dummy,
                        &selector,
                    )
                    .with_context(|| " couldn't constrain `enforce_selector_with_premise`")
                }
            }
        }

        let call_outputs = frame.preimages.call_outputs.clone();
        recurse(
            cs,
            &self.body,
            &Boolean::Constant(true),
            &mut SlotsCounter::default(),
            &mut bound_allocations,
            &preallocated_outputs,
            &mut Globals {
                store,
                global_allocator: &mut global_allocator,
                preallocated_hash2_slots,
                preallocated_hash3_slots,
                preallocated_hash4_slots,
                preallocated_commitment_slots,
                preallocated_less_than_slots,
                call_outputs,
                call_count: 0,
            },
        )
    }

    /// Computes the number of constraints that `synthesize` should create. It's
    /// also an explicit way to document and attest how the number of constraints
    /// grow.
    pub fn num_constraints<F: LurkField>(&self, store: &mut Store<F>) -> usize {
        fn recurse<F: LurkField>(
            block: &Block,
            globals: &mut HashSet<FWrap<F>>,
            store: &mut Store<F>,
        ) -> usize {
            let mut num_constraints = 0;
            for op in &block.ops {
                match op {
                    Op::Call(_, func, _) => {
                        num_constraints += recurse(&func.body, globals, store);
                    }
                    Op::Null(_, tag) => {
                        // constrain tag and hash
                        globals.insert(FWrap(tag.to_field()));
                        globals.insert(FWrap(F::ZERO));
                    }
                    Op::Lit(_, lit) => {
                        let lit_ptr = lit.to_ptr(store);
                        let lit_hash = store.hash_ptr(&lit_ptr).unwrap().hash;
                        globals.insert(FWrap(Tag::Expr(Sym).to_field()));
                        globals.insert(FWrap(lit_hash));
                    }
                    Op::Cast(_tgt, tag, _src) => {
                        globals.insert(FWrap(tag.to_field()));
                    }
                    Op::EqTag(_, _, _) | Op::EqVal(_, _, _) => {
                        globals.insert(FWrap(Tag::Expr(Num).to_field()));
                        num_constraints += 5;
                    }
                    Op::Add(_, _, _) | Op::Sub(_, _, _) | Op::Mul(_, _, _) => {
                        globals.insert(FWrap(Tag::Expr(Num).to_field()));
                        num_constraints += 1;
                    }
                    Op::Div(_, _, _) => {
                        globals.insert(FWrap(F::ONE));
                        num_constraints += 5;
                    }
                    Op::Lt(_, _, _) => {
                        globals.insert(FWrap(Tag::Expr(Num).to_field()));
                        num_constraints += 2;
                    }
                    Op::Trunc(_, _, _) => {
                        globals.insert(FWrap(Tag::Expr(Num).to_field()));
                        // bit decomposition + enforce_pack
                        num_constraints += 389;
                    }
                    Op::DivRem64(_, _, _) => {
                        globals.insert(FWrap(Tag::Expr(Num).to_field()));
                        // three implies_u64, one sub and one linear
                        num_constraints += 197;
                    }
                    Op::Emit(_) => (),
                    Op::Hash2(_, tag, _) => {
                        // tag for the image
                        globals.insert(FWrap(tag.to_field()));
                        // tag and hash for 2 preimage pointers
                        num_constraints += 4;
                    }
                    Op::Hash3(_, tag, _) => {
                        // tag for the image
                        globals.insert(FWrap(tag.to_field()));
                        // tag and hash for 3 preimage pointers
                        num_constraints += 6;
                    }
                    Op::Hash4(_, tag, _) => {
                        // tag for the image
                        globals.insert(FWrap(tag.to_field()));
                        // tag and hash for 4 preimage pointers
                        num_constraints += 8;
                    }
                    Op::Unhash2(..) | Op::Unhash3(..) | Op::Unhash4(..) => {
                        // one constraint for the image's hash
                        num_constraints += 1;
                    }
                    Op::Hide(..) => {
                        num_constraints += 4;
                        globals.insert(FWrap(Tag::Expr(Num).to_field()));
                        globals.insert(FWrap(Tag::Expr(Comm).to_field()));
                    }
                    Op::Open(..) => {
                        num_constraints += 2;
                        globals.insert(FWrap(Tag::Expr(Num).to_field()));
                        globals.insert(FWrap(Tag::Expr(Comm).to_field()));
                    }
                }
            }
            match &block.ctrl {
                Ctrl::Return(vars) => num_constraints + 2 * vars.len(),
                Ctrl::IfEq(_, _, eq_block, else_block) => {
                    num_constraints
                        + 5
                        + recurse(eq_block, globals, store)
                        + recurse(else_block, globals, store)
                }
                Ctrl::MatchTag(_, cases, def) => {
                    // We allocate one boolean per case and constrain it once
                    // per case. Then we add 1 constraint to enforce only one
                    // case was selected
                    num_constraints += 2 * cases.len() + 1;

                    for block in cases.values() {
                        num_constraints += recurse(block, globals, store);
                    }
                    match def {
                        Some(def) => {
                            // constraints for the boolean, the unequalities and the default case
                            num_constraints += 1 + cases.len();
                            num_constraints += recurse(def, globals, store);
                        }
                        None => (),
                    };
                    num_constraints
                }
                Ctrl::MatchVal(_, cases, def) => {
                    num_constraints += 2 * cases.len() + 1;
                    for block in cases.values() {
                        num_constraints += recurse(block, globals, store);
                    }
                    match def {
                        Some(def) => {
                            num_constraints += 1 + cases.len();
                            num_constraints += recurse(def, globals, store);
                        }
                        None => (),
                    };
                    num_constraints
                }
            }
        }
        let globals = &mut HashSet::default();
        // fixed cost for each slot
        let slot_constraints = 289 * self.slot.hash2
            + 337 * self.slot.hash3
            + 388 * self.slot.hash4
            + 265 * self.slot.commitment
            + 391 * self.slot.less_than;
        let num_constraints = recurse::<F>(&self.body, globals, store);
        slot_constraints + num_constraints + globals.len()
    }
}
