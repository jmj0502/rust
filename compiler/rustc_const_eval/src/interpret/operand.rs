//! Functions concerning immediate values and operands, and reading from operands.
//! All high-level functions to read from memory work on operands as sources.

use std::fmt::Write;

use rustc_hir::def::Namespace;
use rustc_middle::ty::layout::{LayoutOf, PrimitiveExt, TyAndLayout};
use rustc_middle::ty::print::{FmtPrinter, PrettyPrinter, Printer};
use rustc_middle::ty::{ConstInt, DelaySpanBugEmitted, Ty};
use rustc_middle::{mir, ty};
use rustc_target::abi::{self, Abi, Align, HasDataLayout, Size, TagEncoding};
use rustc_target::abi::{VariantIdx, Variants};

use super::{
    alloc_range, from_known_layout, mir_assign_valid_types, AllocId, ConstValue, Frame, GlobalId,
    InterpCx, InterpResult, MPlaceTy, Machine, MemPlace, MemPlaceMeta, Place, PlaceTy, Pointer,
    PointerArithmetic, Provenance, Scalar, ScalarMaybeUninit,
};

/// An `Immediate` represents a single immediate self-contained Rust value.
///
/// For optimization of a few very common cases, there is also a representation for a pair of
/// primitive values (`ScalarPair`). It allows Miri to avoid making allocations for checked binary
/// operations and wide pointers. This idea was taken from rustc's codegen.
/// In particular, thanks to `ScalarPair`, arithmetic operations and casts can be entirely
/// defined on `Immediate`, and do not have to work with a `Place`.
#[derive(Copy, Clone, Debug)]
pub enum Immediate<Tag: Provenance = AllocId> {
    /// A single scalar value (must have *initialized* `Scalar` ABI).
    /// FIXME: we also currently often use this for ZST.
    /// `ScalarMaybeUninit` should reject ZST, and we should use `Uninit` for them instead.
    Scalar(ScalarMaybeUninit<Tag>),
    /// A pair of two scalar value (must have `ScalarPair` ABI where both fields are
    /// `Scalar::Initialized`).
    ScalarPair(ScalarMaybeUninit<Tag>, ScalarMaybeUninit<Tag>),
    /// A value of fully uninitialized memory. Can have and size and layout.
    Uninit,
}

#[cfg(all(target_arch = "x86_64", target_pointer_width = "64"))]
rustc_data_structures::static_assert_size!(Immediate, 56);

impl<Tag: Provenance> From<ScalarMaybeUninit<Tag>> for Immediate<Tag> {
    #[inline(always)]
    fn from(val: ScalarMaybeUninit<Tag>) -> Self {
        Immediate::Scalar(val)
    }
}

impl<Tag: Provenance> From<Scalar<Tag>> for Immediate<Tag> {
    #[inline(always)]
    fn from(val: Scalar<Tag>) -> Self {
        Immediate::Scalar(val.into())
    }
}

impl<'tcx, Tag: Provenance> Immediate<Tag> {
    pub fn from_pointer(p: Pointer<Tag>, cx: &impl HasDataLayout) -> Self {
        Immediate::Scalar(ScalarMaybeUninit::from_pointer(p, cx))
    }

    pub fn from_maybe_pointer(p: Pointer<Option<Tag>>, cx: &impl HasDataLayout) -> Self {
        Immediate::Scalar(ScalarMaybeUninit::from_maybe_pointer(p, cx))
    }

    pub fn new_slice(val: Scalar<Tag>, len: u64, cx: &impl HasDataLayout) -> Self {
        Immediate::ScalarPair(val.into(), Scalar::from_machine_usize(len, cx).into())
    }

    pub fn new_dyn_trait(
        val: Scalar<Tag>,
        vtable: Pointer<Option<Tag>>,
        cx: &impl HasDataLayout,
    ) -> Self {
        Immediate::ScalarPair(val.into(), ScalarMaybeUninit::from_maybe_pointer(vtable, cx))
    }

    #[inline]
    #[cfg_attr(debug_assertions, track_caller)] // only in debug builds due to perf (see #98980)
    pub fn to_scalar_or_uninit(self) -> ScalarMaybeUninit<Tag> {
        match self {
            Immediate::Scalar(val) => val,
            Immediate::ScalarPair(..) => bug!("Got a scalar pair where a scalar was expected"),
            Immediate::Uninit => ScalarMaybeUninit::Uninit,
        }
    }

    #[inline]
    #[cfg_attr(debug_assertions, track_caller)] // only in debug builds due to perf (see #98980)
    pub fn to_scalar(self) -> InterpResult<'tcx, Scalar<Tag>> {
        self.to_scalar_or_uninit().check_init()
    }

    #[inline]
    #[cfg_attr(debug_assertions, track_caller)] // only in debug builds due to perf (see #98980)
    pub fn to_scalar_or_uninit_pair(self) -> (ScalarMaybeUninit<Tag>, ScalarMaybeUninit<Tag>) {
        match self {
            Immediate::ScalarPair(val1, val2) => (val1, val2),
            Immediate::Scalar(..) => bug!("Got a scalar where a scalar pair was expected"),
            Immediate::Uninit => (ScalarMaybeUninit::Uninit, ScalarMaybeUninit::Uninit),
        }
    }

    #[inline]
    #[cfg_attr(debug_assertions, track_caller)] // only in debug builds due to perf (see #98980)
    pub fn to_scalar_pair(self) -> InterpResult<'tcx, (Scalar<Tag>, Scalar<Tag>)> {
        let (val1, val2) = self.to_scalar_or_uninit_pair();
        Ok((val1.check_init()?, val2.check_init()?))
    }
}

// ScalarPair needs a type to interpret, so we often have an immediate and a type together
// as input for binary and cast operations.
#[derive(Clone, Debug)]
pub struct ImmTy<'tcx, Tag: Provenance = AllocId> {
    imm: Immediate<Tag>,
    pub layout: TyAndLayout<'tcx>,
}

#[cfg(all(target_arch = "x86_64", target_pointer_width = "64"))]
rustc_data_structures::static_assert_size!(ImmTy<'_>, 72);

impl<Tag: Provenance> std::fmt::Display for ImmTy<'_, Tag> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        /// Helper function for printing a scalar to a FmtPrinter
        fn p<'a, 'tcx, Tag: Provenance>(
            cx: FmtPrinter<'a, 'tcx>,
            s: ScalarMaybeUninit<Tag>,
            ty: Ty<'tcx>,
        ) -> Result<FmtPrinter<'a, 'tcx>, std::fmt::Error> {
            match s {
                ScalarMaybeUninit::Scalar(Scalar::Int(int)) => {
                    cx.pretty_print_const_scalar_int(int, ty, true)
                }
                ScalarMaybeUninit::Scalar(Scalar::Ptr(ptr, _sz)) => {
                    // Just print the ptr value. `pretty_print_const_scalar_ptr` would also try to
                    // print what is points to, which would fail since it has no access to the local
                    // memory.
                    cx.pretty_print_const_pointer(ptr, ty, true)
                }
                ScalarMaybeUninit::Uninit => cx.typed_value(
                    |mut this| {
                        this.write_str("uninit ")?;
                        Ok(this)
                    },
                    |this| this.print_type(ty),
                    " ",
                ),
            }
        }
        ty::tls::with(|tcx| {
            match self.imm {
                Immediate::Scalar(s) => {
                    if let Some(ty) = tcx.lift(self.layout.ty) {
                        let cx = FmtPrinter::new(tcx, Namespace::ValueNS);
                        f.write_str(&p(cx, s, ty)?.into_buffer())?;
                        return Ok(());
                    }
                    write!(f, "{:x}: {}", s, self.layout.ty)
                }
                Immediate::ScalarPair(a, b) => {
                    // FIXME(oli-obk): at least print tuples and slices nicely
                    write!(f, "({:x}, {:x}): {}", a, b, self.layout.ty)
                }
                Immediate::Uninit => {
                    write!(f, "uninit: {}", self.layout.ty)
                }
            }
        })
    }
}

impl<'tcx, Tag: Provenance> std::ops::Deref for ImmTy<'tcx, Tag> {
    type Target = Immediate<Tag>;
    #[inline(always)]
    fn deref(&self) -> &Immediate<Tag> {
        &self.imm
    }
}

/// An `Operand` is the result of computing a `mir::Operand`. It can be immediate,
/// or still in memory. The latter is an optimization, to delay reading that chunk of
/// memory and to avoid having to store arbitrary-sized data here.
#[derive(Copy, Clone, Debug)]
pub enum Operand<Tag: Provenance = AllocId> {
    Immediate(Immediate<Tag>),
    Indirect(MemPlace<Tag>),
}

#[cfg(all(target_arch = "x86_64", target_pointer_width = "64"))]
rustc_data_structures::static_assert_size!(Operand, 64);

#[derive(Clone, Debug)]
pub struct OpTy<'tcx, Tag: Provenance = AllocId> {
    op: Operand<Tag>, // Keep this private; it helps enforce invariants.
    pub layout: TyAndLayout<'tcx>,
    /// rustc does not have a proper way to represent the type of a field of a `repr(packed)` struct:
    /// it needs to have a different alignment than the field type would usually have.
    /// So we represent this here with a separate field that "overwrites" `layout.align`.
    /// This means `layout.align` should never be used for an `OpTy`!
    /// `None` means "alignment does not matter since this is a by-value operand"
    /// (`Operand::Immediate`); this field is only relevant for `Operand::Indirect`.
    /// Also CTFE ignores alignment anyway, so this is for Miri only.
    pub align: Option<Align>,
}

#[cfg(all(target_arch = "x86_64", target_pointer_width = "64"))]
rustc_data_structures::static_assert_size!(OpTy<'_>, 88);

impl<'tcx, Tag: Provenance> std::ops::Deref for OpTy<'tcx, Tag> {
    type Target = Operand<Tag>;
    #[inline(always)]
    fn deref(&self) -> &Operand<Tag> {
        &self.op
    }
}

impl<'tcx, Tag: Provenance> From<MPlaceTy<'tcx, Tag>> for OpTy<'tcx, Tag> {
    #[inline(always)]
    fn from(mplace: MPlaceTy<'tcx, Tag>) -> Self {
        OpTy { op: Operand::Indirect(*mplace), layout: mplace.layout, align: Some(mplace.align) }
    }
}

impl<'tcx, Tag: Provenance> From<&'_ MPlaceTy<'tcx, Tag>> for OpTy<'tcx, Tag> {
    #[inline(always)]
    fn from(mplace: &MPlaceTy<'tcx, Tag>) -> Self {
        OpTy { op: Operand::Indirect(**mplace), layout: mplace.layout, align: Some(mplace.align) }
    }
}

impl<'tcx, Tag: Provenance> From<&'_ mut MPlaceTy<'tcx, Tag>> for OpTy<'tcx, Tag> {
    #[inline(always)]
    fn from(mplace: &mut MPlaceTy<'tcx, Tag>) -> Self {
        OpTy { op: Operand::Indirect(**mplace), layout: mplace.layout, align: Some(mplace.align) }
    }
}

impl<'tcx, Tag: Provenance> From<ImmTy<'tcx, Tag>> for OpTy<'tcx, Tag> {
    #[inline(always)]
    fn from(val: ImmTy<'tcx, Tag>) -> Self {
        OpTy { op: Operand::Immediate(val.imm), layout: val.layout, align: None }
    }
}

impl<'tcx, Tag: Provenance> ImmTy<'tcx, Tag> {
    #[inline]
    pub fn from_scalar(val: Scalar<Tag>, layout: TyAndLayout<'tcx>) -> Self {
        ImmTy { imm: val.into(), layout }
    }

    #[inline]
    pub fn from_immediate(imm: Immediate<Tag>, layout: TyAndLayout<'tcx>) -> Self {
        ImmTy { imm, layout }
    }

    #[inline]
    pub fn uninit(layout: TyAndLayout<'tcx>) -> Self {
        ImmTy { imm: Immediate::Uninit, layout }
    }

    #[inline]
    pub fn try_from_uint(i: impl Into<u128>, layout: TyAndLayout<'tcx>) -> Option<Self> {
        Some(Self::from_scalar(Scalar::try_from_uint(i, layout.size)?, layout))
    }
    #[inline]
    pub fn from_uint(i: impl Into<u128>, layout: TyAndLayout<'tcx>) -> Self {
        Self::from_scalar(Scalar::from_uint(i, layout.size), layout)
    }

    #[inline]
    pub fn try_from_int(i: impl Into<i128>, layout: TyAndLayout<'tcx>) -> Option<Self> {
        Some(Self::from_scalar(Scalar::try_from_int(i, layout.size)?, layout))
    }

    #[inline]
    pub fn from_int(i: impl Into<i128>, layout: TyAndLayout<'tcx>) -> Self {
        Self::from_scalar(Scalar::from_int(i, layout.size), layout)
    }

    #[inline]
    pub fn to_const_int(self) -> ConstInt {
        assert!(self.layout.ty.is_integral());
        let int = self.to_scalar().expect("to_const_int doesn't work on scalar pairs").assert_int();
        ConstInt::new(int, self.layout.ty.is_signed(), self.layout.ty.is_ptr_sized_integral())
    }
}

impl<'tcx, Tag: Provenance> OpTy<'tcx, Tag> {
    pub fn len(&self, cx: &impl HasDataLayout) -> InterpResult<'tcx, u64> {
        if self.layout.is_unsized() {
            // There are no unsized immediates.
            self.assert_mem_place().len(cx)
        } else {
            match self.layout.fields {
                abi::FieldsShape::Array { count, .. } => Ok(count),
                _ => bug!("len not supported on sized type {:?}", self.layout.ty),
            }
        }
    }

    pub fn offset_with_meta(
        &self,
        offset: Size,
        meta: MemPlaceMeta<Tag>,
        layout: TyAndLayout<'tcx>,
        cx: &impl HasDataLayout,
    ) -> InterpResult<'tcx, Self> {
        match self.try_as_mplace() {
            Ok(mplace) => Ok(mplace.offset_with_meta(offset, meta, layout, cx)?.into()),
            Err(imm) => {
                assert!(
                    matches!(*imm, Immediate::Uninit),
                    "Scalar/ScalarPair cannot be offset into"
                );
                assert!(!meta.has_meta()); // no place to store metadata here
                // Every part of an uninit is uninit.
                Ok(ImmTy::uninit(layout).into())
            }
        }
    }

    pub fn offset(
        &self,
        offset: Size,
        layout: TyAndLayout<'tcx>,
        cx: &impl HasDataLayout,
    ) -> InterpResult<'tcx, Self> {
        assert!(!layout.is_unsized());
        self.offset_with_meta(offset, MemPlaceMeta::None, layout, cx)
    }
}

impl<'mir, 'tcx: 'mir, M: Machine<'mir, 'tcx>> InterpCx<'mir, 'tcx, M> {
    /// Try reading an immediate in memory; this is interesting particularly for `ScalarPair`.
    /// Returns `None` if the layout does not permit loading this as a value.
    ///
    /// This is an internal function; call `read_immediate` instead.
    fn read_immediate_from_mplace_raw(
        &self,
        mplace: &MPlaceTy<'tcx, M::PointerTag>,
        force: bool,
    ) -> InterpResult<'tcx, Option<ImmTy<'tcx, M::PointerTag>>> {
        if mplace.layout.is_unsized() {
            // Don't touch unsized
            return Ok(None);
        }

        let Some(alloc) = self.get_place_alloc(mplace)? else {
            // zero-sized type can be left uninit
            return Ok(Some(ImmTy::uninit(mplace.layout)));
        };

        // It may seem like all types with `Scalar` or `ScalarPair` ABI are fair game at this point.
        // However, `MaybeUninit<u64>` is considered a `Scalar` as far as its layout is concerned --
        // and yet cannot be represented by an interpreter `Scalar`, since we have to handle the
        // case where some of the bytes are initialized and others are not. So, we need an extra
        // check that walks over the type of `mplace` to make sure it is truly correct to treat this
        // like a `Scalar` (or `ScalarPair`).
        let scalar_layout = match mplace.layout.abi {
            // `if` does not work nested inside patterns, making this a bit awkward to express.
            Abi::Scalar(abi::Scalar::Initialized { value: s, .. }) => Some(s),
            Abi::Scalar(s) if force => Some(s.primitive()),
            _ => None,
        };
        let read_provenance = |s: abi::Primitive, size| {
            // Should be just `s.is_ptr()`, but we support a Miri flag that accepts more
            // questionable ptr-int transmutes.
            let number_may_have_provenance = !M::enforce_number_no_provenance(self);
            s.is_ptr() || (number_may_have_provenance && size == self.pointer_size())
        };
        if let Some(s) = scalar_layout {
            let size = s.size(self);
            assert_eq!(size, mplace.layout.size, "abi::Scalar size does not match layout size");
            let scalar =
                alloc.read_scalar(alloc_range(Size::ZERO, size), read_provenance(s, size))?;
            return Ok(Some(ImmTy { imm: scalar.into(), layout: mplace.layout }));
        }
        let scalar_pair_layout = match mplace.layout.abi {
            Abi::ScalarPair(
                abi::Scalar::Initialized { value: a, .. },
                abi::Scalar::Initialized { value: b, .. },
            ) => Some((a, b)),
            Abi::ScalarPair(a, b) if force => Some((a.primitive(), b.primitive())),
            _ => None,
        };
        if let Some((a, b)) = scalar_pair_layout {
            // We checked `ptr_align` above, so all fields will have the alignment they need.
            // We would anyway check against `ptr_align.restrict_for_offset(b_offset)`,
            // which `ptr.offset(b_offset)` cannot possibly fail to satisfy.
            let (a_size, b_size) = (a.size(self), b.size(self));
            let b_offset = a_size.align_to(b.align(self).abi);
            assert!(b_offset.bytes() > 0); // in `operand_field` we use the offset to tell apart the fields
            let a_val =
                alloc.read_scalar(alloc_range(Size::ZERO, a_size), read_provenance(a, a_size))?;
            let b_val =
                alloc.read_scalar(alloc_range(b_offset, b_size), read_provenance(b, b_size))?;
            return Ok(Some(ImmTy {
                imm: Immediate::ScalarPair(a_val, b_val),
                layout: mplace.layout,
            }));
        }
        // Neither a scalar nor scalar pair.
        return Ok(None);
    }

    /// Try returning an immediate for the operand. If the layout does not permit loading this as an
    /// immediate, return where in memory we can find the data.
    /// Note that for a given layout, this operation will either always fail or always
    /// succeed!  Whether it succeeds depends on whether the layout can be represented
    /// in an `Immediate`, not on which data is stored there currently.
    ///
    /// If `force` is `true`, then even scalars with fields that can be ununit will be
    /// read. This means the load is lossy and should not be written back!
    /// This flag exists only for validity checking.
    ///
    /// This is an internal function that should not usually be used; call `read_immediate` instead.
    /// ConstProp needs it, though.
    pub fn read_immediate_raw(
        &self,
        src: &OpTy<'tcx, M::PointerTag>,
        force: bool,
    ) -> InterpResult<'tcx, Result<ImmTy<'tcx, M::PointerTag>, MPlaceTy<'tcx, M::PointerTag>>> {
        Ok(match src.try_as_mplace() {
            Ok(ref mplace) => {
                if let Some(val) = self.read_immediate_from_mplace_raw(mplace, force)? {
                    Ok(val)
                } else {
                    Err(*mplace)
                }
            }
            Err(val) => Ok(val),
        })
    }

    /// Read an immediate from a place, asserting that that is possible with the given layout.
    #[inline(always)]
    pub fn read_immediate(
        &self,
        op: &OpTy<'tcx, M::PointerTag>,
    ) -> InterpResult<'tcx, ImmTy<'tcx, M::PointerTag>> {
        if let Ok(imm) = self.read_immediate_raw(op, /*force*/ false)? {
            Ok(imm)
        } else {
            span_bug!(self.cur_span(), "primitive read failed for type: {:?}", op.layout.ty);
        }
    }

    /// Read a scalar from a place
    pub fn read_scalar(
        &self,
        op: &OpTy<'tcx, M::PointerTag>,
    ) -> InterpResult<'tcx, ScalarMaybeUninit<M::PointerTag>> {
        Ok(self.read_immediate(op)?.to_scalar_or_uninit())
    }

    /// Read a pointer from a place.
    pub fn read_pointer(
        &self,
        op: &OpTy<'tcx, M::PointerTag>,
    ) -> InterpResult<'tcx, Pointer<Option<M::PointerTag>>> {
        self.scalar_to_ptr(self.read_scalar(op)?.check_init()?)
    }

    /// Turn the wide MPlace into a string (must already be dereferenced!)
    pub fn read_str(&self, mplace: &MPlaceTy<'tcx, M::PointerTag>) -> InterpResult<'tcx, &str> {
        let len = mplace.len(self)?;
        let bytes = self.read_bytes_ptr(mplace.ptr, Size::from_bytes(len))?;
        let str = std::str::from_utf8(bytes).map_err(|err| err_ub!(InvalidStr(err)))?;
        Ok(str)
    }

    /// Converts a repr(simd) operand into an operand where `place_index` accesses the SIMD elements.
    /// Also returns the number of elements.
    ///
    /// Can (but does not always) trigger UB if `op` is uninitialized.
    pub fn operand_to_simd(
        &self,
        op: &OpTy<'tcx, M::PointerTag>,
    ) -> InterpResult<'tcx, (MPlaceTy<'tcx, M::PointerTag>, u64)> {
        // Basically we just transmute this place into an array following simd_size_and_type.
        // This only works in memory, but repr(simd) types should never be immediates anyway.
        assert!(op.layout.ty.is_simd());
        match op.try_as_mplace() {
            Ok(mplace) => self.mplace_to_simd(&mplace),
            Err(imm) => match *imm {
                Immediate::Uninit => {
                    throw_ub!(InvalidUninitBytes(None))
                }
                Immediate::Scalar(..) | Immediate::ScalarPair(..) => {
                    bug!("arrays/slices can never have Scalar/ScalarPair layout")
                }
            },
        }
    }

    /// Read from a local. Will not actually access the local if reading from a ZST.
    /// Will not access memory, instead an indirect `Operand` is returned.
    ///
    /// This is public because it is used by [priroda](https://github.com/oli-obk/priroda) to get an
    /// OpTy from a local.
    pub fn local_to_op(
        &self,
        frame: &Frame<'mir, 'tcx, M::PointerTag, M::FrameExtra>,
        local: mir::Local,
        layout: Option<TyAndLayout<'tcx>>,
    ) -> InterpResult<'tcx, OpTy<'tcx, M::PointerTag>> {
        let layout = self.layout_of_local(frame, local, layout)?;
        let op = if layout.is_zst() {
            // Bypass `access_local` (helps in ConstProp)
            Operand::Immediate(Immediate::Uninit)
        } else {
            *M::access_local(frame, local)?
        };
        Ok(OpTy { op, layout, align: Some(layout.align.abi) })
    }

    /// Every place can be read from, so we can turn them into an operand.
    /// This will definitely return `Indirect` if the place is a `Ptr`, i.e., this
    /// will never actually read from memory.
    #[inline(always)]
    pub fn place_to_op(
        &self,
        place: &PlaceTy<'tcx, M::PointerTag>,
    ) -> InterpResult<'tcx, OpTy<'tcx, M::PointerTag>> {
        let op = match **place {
            Place::Ptr(mplace) => Operand::Indirect(mplace),
            Place::Local { frame, local } => {
                *self.local_to_op(&self.stack()[frame], local, None)?
            }
        };
        Ok(OpTy { op, layout: place.layout, align: Some(place.align) })
    }

    /// Evaluate a place with the goal of reading from it.  This lets us sometimes
    /// avoid allocations.
    pub fn eval_place_to_op(
        &self,
        mir_place: mir::Place<'tcx>,
        layout: Option<TyAndLayout<'tcx>>,
    ) -> InterpResult<'tcx, OpTy<'tcx, M::PointerTag>> {
        // Do not use the layout passed in as argument if the base we are looking at
        // here is not the entire place.
        let layout = if mir_place.projection.is_empty() { layout } else { None };

        let mut op = self.local_to_op(self.frame(), mir_place.local, layout)?;
        // Using `try_fold` turned out to be bad for performance, hence the loop.
        for elem in mir_place.projection.iter() {
            op = self.operand_projection(&op, elem)?
        }

        trace!("eval_place_to_op: got {:?}", *op);
        // Sanity-check the type we ended up with.
        debug_assert!(
            mir_assign_valid_types(
                *self.tcx,
                self.param_env,
                self.layout_of(self.subst_from_current_frame_and_normalize_erasing_regions(
                    mir_place.ty(&self.frame().body.local_decls, *self.tcx).ty
                )?)?,
                op.layout,
            ),
            "eval_place of a MIR place with type {:?} produced an interpreter operand with type {:?}",
            mir_place.ty(&self.frame().body.local_decls, *self.tcx).ty,
            op.layout.ty,
        );
        Ok(op)
    }

    /// Evaluate the operand, returning a place where you can then find the data.
    /// If you already know the layout, you can save two table lookups
    /// by passing it in here.
    #[inline]
    pub fn eval_operand(
        &self,
        mir_op: &mir::Operand<'tcx>,
        layout: Option<TyAndLayout<'tcx>>,
    ) -> InterpResult<'tcx, OpTy<'tcx, M::PointerTag>> {
        use rustc_middle::mir::Operand::*;
        let op = match *mir_op {
            // FIXME: do some more logic on `move` to invalidate the old location
            Copy(place) | Move(place) => self.eval_place_to_op(place, layout)?,

            Constant(ref constant) => {
                let val =
                    self.subst_from_current_frame_and_normalize_erasing_regions(constant.literal)?;

                // This can still fail:
                // * During ConstProp, with `TooGeneric` or since the `required_consts` were not all
                //   checked yet.
                // * During CTFE, since promoteds in `const`/`static` initializer bodies can fail.
                self.mir_const_to_op(&val, layout)?
            }
        };
        trace!("{:?}: {:?}", mir_op, *op);
        Ok(op)
    }

    /// Evaluate a bunch of operands at once
    pub(super) fn eval_operands(
        &self,
        ops: &[mir::Operand<'tcx>],
    ) -> InterpResult<'tcx, Vec<OpTy<'tcx, M::PointerTag>>> {
        ops.iter().map(|op| self.eval_operand(op, None)).collect()
    }

    // Used when the miri-engine runs into a constant and for extracting information from constants
    // in patterns via the `const_eval` module
    /// The `val` and `layout` are assumed to already be in our interpreter
    /// "universe" (param_env).
    pub fn const_to_op(
        &self,
        c: ty::Const<'tcx>,
        layout: Option<TyAndLayout<'tcx>>,
    ) -> InterpResult<'tcx, OpTy<'tcx, M::PointerTag>> {
        match c.kind() {
            ty::ConstKind::Param(_) | ty::ConstKind::Bound(..) => throw_inval!(TooGeneric),
            ty::ConstKind::Error(DelaySpanBugEmitted { reported, .. }) => {
                throw_inval!(AlreadyReported(reported))
            }
            ty::ConstKind::Unevaluated(uv) => {
                let instance = self.resolve(uv.def, uv.substs)?;
                Ok(self.eval_to_allocation(GlobalId { instance, promoted: uv.promoted })?.into())
            }
            ty::ConstKind::Infer(..) | ty::ConstKind::Placeholder(..) => {
                span_bug!(self.cur_span(), "const_to_op: Unexpected ConstKind {:?}", c)
            }
            ty::ConstKind::Value(valtree) => {
                let ty = c.ty();
                let const_val = self.tcx.valtree_to_const_val((ty, valtree));
                self.const_val_to_op(const_val, ty, layout)
            }
        }
    }

    pub fn mir_const_to_op(
        &self,
        val: &mir::ConstantKind<'tcx>,
        layout: Option<TyAndLayout<'tcx>>,
    ) -> InterpResult<'tcx, OpTy<'tcx, M::PointerTag>> {
        match val {
            mir::ConstantKind::Ty(ct) => self.const_to_op(*ct, layout),
            mir::ConstantKind::Val(val, ty) => self.const_val_to_op(*val, *ty, layout),
        }
    }

    pub(crate) fn const_val_to_op(
        &self,
        val_val: ConstValue<'tcx>,
        ty: Ty<'tcx>,
        layout: Option<TyAndLayout<'tcx>>,
    ) -> InterpResult<'tcx, OpTy<'tcx, M::PointerTag>> {
        // Other cases need layout.
        let tag_scalar = |scalar| -> InterpResult<'tcx, _> {
            Ok(match scalar {
                Scalar::Ptr(ptr, size) => Scalar::Ptr(self.global_base_pointer(ptr)?, size),
                Scalar::Int(int) => Scalar::Int(int),
            })
        };
        let layout = from_known_layout(self.tcx, self.param_env, layout, || self.layout_of(ty))?;
        let op = match val_val {
            ConstValue::ByRef { alloc, offset } => {
                let id = self.tcx.create_memory_alloc(alloc);
                // We rely on mutability being set correctly in that allocation to prevent writes
                // where none should happen.
                let ptr = self.global_base_pointer(Pointer::new(id, offset))?;
                Operand::Indirect(MemPlace::from_ptr(ptr.into()))
            }
            ConstValue::Scalar(x) => Operand::Immediate(tag_scalar(x)?.into()),
            ConstValue::ZeroSized => Operand::Immediate(Immediate::Uninit),
            ConstValue::Slice { data, start, end } => {
                // We rely on mutability being set correctly in `data` to prevent writes
                // where none should happen.
                let ptr = Pointer::new(
                    self.tcx.create_memory_alloc(data),
                    Size::from_bytes(start), // offset: `start`
                );
                Operand::Immediate(Immediate::new_slice(
                    Scalar::from_pointer(self.global_base_pointer(ptr)?, &*self.tcx),
                    u64::try_from(end.checked_sub(start).unwrap()).unwrap(), // len: `end - start`
                    self,
                ))
            }
        };
        Ok(OpTy { op, layout, align: Some(layout.align.abi) })
    }

    /// Read discriminant, return the runtime value as well as the variant index.
    /// Can also legally be called on non-enums (e.g. through the discriminant_value intrinsic)!
    pub fn read_discriminant(
        &self,
        op: &OpTy<'tcx, M::PointerTag>,
    ) -> InterpResult<'tcx, (Scalar<M::PointerTag>, VariantIdx)> {
        trace!("read_discriminant_value {:#?}", op.layout);
        // Get type and layout of the discriminant.
        let discr_layout = self.layout_of(op.layout.ty.discriminant_ty(*self.tcx))?;
        trace!("discriminant type: {:?}", discr_layout.ty);

        // We use "discriminant" to refer to the value associated with a particular enum variant.
        // This is not to be confused with its "variant index", which is just determining its position in the
        // declared list of variants -- they can differ with explicitly assigned discriminants.
        // We use "tag" to refer to how the discriminant is encoded in memory, which can be either
        // straight-forward (`TagEncoding::Direct`) or with a niche (`TagEncoding::Niche`).
        let (tag_scalar_layout, tag_encoding, tag_field) = match op.layout.variants {
            Variants::Single { index } => {
                let discr = match op.layout.ty.discriminant_for_variant(*self.tcx, index) {
                    Some(discr) => {
                        // This type actually has discriminants.
                        assert_eq!(discr.ty, discr_layout.ty);
                        Scalar::from_uint(discr.val, discr_layout.size)
                    }
                    None => {
                        // On a type without actual discriminants, variant is 0.
                        assert_eq!(index.as_u32(), 0);
                        Scalar::from_uint(index.as_u32(), discr_layout.size)
                    }
                };
                return Ok((discr, index));
            }
            Variants::Multiple { tag, ref tag_encoding, tag_field, .. } => {
                (tag, tag_encoding, tag_field)
            }
        };

        // There are *three* layouts that come into play here:
        // - The discriminant has a type for typechecking. This is `discr_layout`, and is used for
        //   the `Scalar` we return.
        // - The tag (encoded discriminant) has layout `tag_layout`. This is always an integer type,
        //   and used to interpret the value we read from the tag field.
        //   For the return value, a cast to `discr_layout` is performed.
        // - The field storing the tag has a layout, which is very similar to `tag_layout` but
        //   may be a pointer. This is `tag_val.layout`; we just use it for sanity checks.

        // Get layout for tag.
        let tag_layout = self.layout_of(tag_scalar_layout.primitive().to_int_ty(*self.tcx))?;

        // Read tag and sanity-check `tag_layout`.
        let tag_val = self.read_immediate(&self.operand_field(op, tag_field)?)?;
        assert_eq!(tag_layout.size, tag_val.layout.size);
        assert_eq!(tag_layout.abi.is_signed(), tag_val.layout.abi.is_signed());
        trace!("tag value: {}", tag_val);

        // Figure out which discriminant and variant this corresponds to.
        Ok(match *tag_encoding {
            TagEncoding::Direct => {
                let scalar = tag_val.to_scalar()?;
                // Generate a specific error if `tag_val` is not an integer.
                // (`tag_bits` itself is only used for error messages below.)
                let tag_bits = scalar
                    .try_to_int()
                    .map_err(|dbg_val| err_ub!(InvalidTag(dbg_val)))?
                    .assert_bits(tag_layout.size);
                // Cast bits from tag layout to discriminant layout.
                // After the checks we did above, this cannot fail, as
                // discriminants are int-like.
                let discr_val =
                    self.cast_from_int_like(scalar, tag_val.layout, discr_layout.ty).unwrap();
                let discr_bits = discr_val.assert_bits(discr_layout.size);
                // Convert discriminant to variant index, and catch invalid discriminants.
                let index = match *op.layout.ty.kind() {
                    ty::Adt(adt, _) => {
                        adt.discriminants(*self.tcx).find(|(_, var)| var.val == discr_bits)
                    }
                    ty::Generator(def_id, substs, _) => {
                        let substs = substs.as_generator();
                        substs
                            .discriminants(def_id, *self.tcx)
                            .find(|(_, var)| var.val == discr_bits)
                    }
                    _ => span_bug!(self.cur_span(), "tagged layout for non-adt non-generator"),
                }
                .ok_or_else(|| err_ub!(InvalidTag(Scalar::from_uint(tag_bits, tag_layout.size))))?;
                // Return the cast value, and the index.
                (discr_val, index.0)
            }
            TagEncoding::Niche { dataful_variant, ref niche_variants, niche_start } => {
                let tag_val = tag_val.to_scalar()?;
                // Compute the variant this niche value/"tag" corresponds to. With niche layout,
                // discriminant (encoded in niche/tag) and variant index are the same.
                let variants_start = niche_variants.start().as_u32();
                let variants_end = niche_variants.end().as_u32();
                let variant = match tag_val.try_to_int() {
                    Err(dbg_val) => {
                        // So this is a pointer then, and casting to an int failed.
                        // Can only happen during CTFE.
                        // The niche must be just 0, and the ptr not null, then we know this is
                        // okay. Everything else, we conservatively reject.
                        let ptr_valid = niche_start == 0
                            && variants_start == variants_end
                            && !self.scalar_may_be_null(tag_val)?;
                        if !ptr_valid {
                            throw_ub!(InvalidTag(dbg_val))
                        }
                        dataful_variant
                    }
                    Ok(tag_bits) => {
                        let tag_bits = tag_bits.assert_bits(tag_layout.size);
                        // We need to use machine arithmetic to get the relative variant idx:
                        // variant_index_relative = tag_val - niche_start_val
                        let tag_val = ImmTy::from_uint(tag_bits, tag_layout);
                        let niche_start_val = ImmTy::from_uint(niche_start, tag_layout);
                        let variant_index_relative_val =
                            self.binary_op(mir::BinOp::Sub, &tag_val, &niche_start_val)?;
                        let variant_index_relative = variant_index_relative_val
                            .to_scalar()?
                            .assert_bits(tag_val.layout.size);
                        // Check if this is in the range that indicates an actual discriminant.
                        if variant_index_relative <= u128::from(variants_end - variants_start) {
                            let variant_index_relative = u32::try_from(variant_index_relative)
                                .expect("we checked that this fits into a u32");
                            // Then computing the absolute variant idx should not overflow any more.
                            let variant_index = variants_start
                                .checked_add(variant_index_relative)
                                .expect("overflow computing absolute variant idx");
                            let variants_len = op
                                .layout
                                .ty
                                .ty_adt_def()
                                .expect("tagged layout for non adt")
                                .variants()
                                .len();
                            assert!(usize::try_from(variant_index).unwrap() < variants_len);
                            VariantIdx::from_u32(variant_index)
                        } else {
                            dataful_variant
                        }
                    }
                };
                // Compute the size of the scalar we need to return.
                // No need to cast, because the variant index directly serves as discriminant and is
                // encoded in the tag.
                (Scalar::from_uint(variant.as_u32(), discr_layout.size), variant)
            }
        })
    }
}
