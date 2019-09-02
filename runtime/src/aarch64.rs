use std::collections::HashMap;
use std::collections::hash_map::Entry::*;
use std::iter::Extend;
use std::mem;
use std::io;

use byteorder::{ByteOrder, LittleEndian};
use take_mut;

use ::{DynasmApi, DynasmLabelApi, LabelRegistry, DynasmError};
use ::common::{BaseAssembler, UncommittedModifier};
use ::{ExecutableBuffer, MutableBuffer, Executor, DynamicLabel, AssemblyOffset};

#[derive(Debug, Clone, Copy)]
enum RelocationType {
    // b, bl 26 bits, dword aligned
    B = 0,
    // b.cond, cbnz, cbz, ldr, ldrsw, prfm: 19 bits, dword aligned
    BCOND = 1,
    // adr split 21 bit, byte aligned
    ADR = 2,
    // adrp split 21 bit, 4096-byte aligned
    ADRP = 3,
    // tbnz, tbz: 14 bits, dword aligned
    TBZ = 4,
    // 32-bit literal
    LITERAL32 = 5,
    // 64-bit literal
    LITERAL64 = 6,
}

impl RelocationType {
    fn from_tuple((type_, ): (u8,)) -> Self {
        match type_ {
            0 => RelocationType::B,
            1 => RelocationType::BCOND,
            2 => RelocationType::ADR,
            3 => RelocationType::ADRP,
            4 => RelocationType::TBZ,
            5 => RelocationType::LITERAL32,
            6 => RelocationType::LITERAL64,
            x => panic!("Unsupported relocation type {}", x)
        }
    }

    fn size(&self) -> usize {
        match self {
            RelocationType::B |
            RelocationType::BCOND |
            RelocationType::ADR |
            RelocationType::ADRP |
            RelocationType::TBZ |
            RelocationType::LITERAL32 => 4,
            RelocationType::LITERAL64 => 8,
        }
    }
}

#[derive(Debug)]
struct PatchLoc(usize, RelocationType);

/// This struct is an implementation of a dynasm runtime. It supports incremental
/// compilation as well as multithreaded execution with simultaneous compilation.
/// Its implementation ensures that no memory is writeable and executable at the
/// same time.
#[derive(Debug)]
pub struct Assembler {
    // protection swapping executable buffer
    base: BaseAssembler,

    // label data storage
    labels: LabelRegistry,

    // end of patch location -> name
    global_relocs: Vec<(PatchLoc, &'static str)>,
    // location to be resolved, loc, label id
    dynamic_relocs: Vec<(PatchLoc, DynamicLabel)>,
    // locations to be patched once this label gets seen. name -> Vec<locs>
    local_relocs: HashMap<&'static str, Vec<PatchLoc>>
}

/// the default starting size for an allocation by this assembler.
/// This is the smallest page size on aarch64 platforms.
const MMAP_INIT_SIZE: usize = 4096;

impl Assembler {
    /// Create a new `Assembler` instance
    /// This function will return an error if it was not
    /// able to map the required executable memory. However, further methods
    /// on the `Assembler` will simply panic if an error occurs during memory
    /// remapping as otherwise it would violate the invariants of the assembler.
    /// This behaviour could be improved but currently the underlying memmap crate
    /// does not return the original mappings if a call to mprotect/VirtualProtect
    /// fails so there is no reliable way to error out if a call fails while leaving
    /// the logic of the `Assembler` intact.
    pub fn new() -> io::Result<Assembler> {
        Ok(Assembler {
            base: BaseAssembler::new(MMAP_INIT_SIZE)?,
            labels: LabelRegistry::new(),
            global_relocs: Vec::new(),
            dynamic_relocs: Vec::new(),
            local_relocs: HashMap::new()
        })
    }

    /// Create a new dynamic label that can be referenced and defined.
    pub fn new_dynamic_label(&mut self) -> DynamicLabel {
        self.labels.new_dynamic_label()
    }

    /// To allow already committed code to be altered, this method allows modification
    /// of the internal ExecutableBuffer directly. When this method is called, all
    /// data will be committed and access to the internal `ExecutableBuffer` will be locked.
    /// The passed function will then be called with an `AssemblyModifier` as argument.
    /// Using this `AssemblyModifier` changes can be made to the committed code.
    /// After this function returns, any labels in these changes will be resolved
    /// and the `ExecutableBuffer` will be unlocked again.
    pub fn alter<F, O>(&mut self, f: F) -> O
    where
        F: FnOnce(&mut AssemblyModifier) -> O
    {
        self.commit();

        let cloned = self.base.reader();
        let mut lock = cloned.write().unwrap();
        let mut out = None;

        // move the buffer out of the assembler for a bit
        // no commit is required afterwards as we directly modified the buffer.
        take_mut::take_or_recover(&mut *lock, || ExecutableBuffer::new(0, MMAP_INIT_SIZE).unwrap(), |buf| {
            let mut buf = buf.make_mut().unwrap();

            {
                let mut m = AssemblyModifier {
                    asmoffset: 0,
                    assembler: self,
                    buffer: &mut buf
                };
                out = Some(f(&mut m));
                m.encode_relocs();
            }

            // and stuff it back in
            buf.make_exec().unwrap()
        });

        out.expect("Programmer error: `take_or_recover` didn't initialize `out`. This is a bug!")
    }

    /// Similar to `Assembler::alter`, this method allows modification of the yet to be
    /// committed assembing buffer. Note that it is not possible to use labels in this
    /// context, and overriding labels will cause corruption when the assembler tries to
    /// resolve the labels at commit time.
    pub fn alter_uncommitted(&mut self) -> UncommittedModifier {
        self.base.alter_uncommitted()
    }

    #[inline]
    fn patch_loc(&mut self, loc: PatchLoc, target: usize) {
        // calculate the offset that the relocation starts at
        // in the executable buffer
        let offset = loc.0 - loc.1.size();

        // the value that the relocation will have
        let t = target.wrapping_sub(offset);

        // write the relocation
        let offset = offset - self.base.asmoffset();
        let buf = &mut self.base.ops[offset .. offset + loc.1.size()];

        // handle non-bitfield variants
        let mask = match loc.1 {
            RelocationType::LITERAL32 => {
                LittleEndian::write_u32(buf, t as u32);
                return;
            },
            RelocationType::LITERAL64 => {
                LittleEndian::write_u64(buf, t as u64);
                return;
            },
            RelocationType::B => 0xFC000000,
            RelocationType::BCOND => 0xFF00001F,
            RelocationType::ADR => 0x9F00001F,
            RelocationType::ADRP => 0x9F00001F,
            RelocationType::TBZ => 0xFFF8001F,
        };

        let base = LittleEndian::read_u32(buf) & mask;
        let t = t as u32;

        let patch = match loc.1 {
            RelocationType::B => (t >> 2) & 0x3FFFFFF,
            RelocationType::BCOND => ((t >> 2) & 0x7FFFF) << 5,
            RelocationType::ADR => ((t & 0x3) << 29) | (((t >> 2) & 0x7FFFF) << 5),
            RelocationType::ADRP => (((t >> 12) & 0x3) << 29) | (((t >> 14) & 0x7FFFF) << 5),
            RelocationType::TBZ => ((t >> 2) & 0x3FFF) << 5,
            RelocationType::LITERAL32 |
            RelocationType::LITERAL64 => unreachable!(),
        };

        LittleEndian::write_u32(buf, base | patch);
    }

    fn encode_relocs(&mut self) {
        let mut relocs = Vec::new();
        mem::swap(&mut relocs, &mut self.global_relocs);
        for (loc, name) in relocs {
            let target = self.labels.resolve_global(name).unwrap();
            self.patch_loc(loc, target.0);
        }

        let mut relocs = Vec::new();
        mem::swap(&mut relocs, &mut self.dynamic_relocs);
        for (loc, id) in relocs {
            let target = self.labels.resolve_dynamic(id).unwrap();
            self.patch_loc(loc, target.0);
        }

        if let Some(name) = self.local_relocs.keys().next() {
            panic!("Unknown local label '{}'", name);
        }
    }

    /// Commit the assembled code from a temporary buffer to the executable buffer.
    /// This method requires write access to the execution buffer and therefore
    /// has to obtain a lock on the datastructure. When this method is called, all
    /// labels will be resolved, and the result can no longer be changed.
    pub fn commit(&mut self) {
        // finalize all relocs in the newest part.
        self.encode_relocs();

        // update the executable buffer
        self.base.commit(|_,_,_|());
    }

    /// Consumes the assembler to return the internal ExecutableBuffer. This
    /// method will only fail if an `Executor` currently holds a lock on the datastructure,
    /// in which case it will return itself.
    pub fn finalize(mut self) -> Result<ExecutableBuffer, Assembler> {
        self.commit();
        match self.base.finalize() {
            Ok(execbuffer) => Ok(execbuffer),
            Err(base) => Err(Assembler {
                base: base,
                ..self
            })
        }
    }

    /// Creates a read-only reference to the internal `ExecutableBuffer` that must
    /// be locked to access it. Multiple of such read-only locks can be obtained
    /// at the same time, but as long as they are alive they will block any `self.commit()`
    /// calls.
    pub fn reader(&self) -> Executor {
        Executor {
            execbuffer: self.base.reader()
        }
    }
}

impl DynasmApi for Assembler {
    #[inline]
    fn offset(&self) -> AssemblyOffset {
        AssemblyOffset(self.base.offset())
    }

    #[inline]
    fn push(&mut self, value: u8) {
        self.base.push(value);
    }

    #[inline]
    fn align(&mut self, alignment: usize) {
        self.base.align(alignment, 0xCC); // TODO: try to align with NOPs
    }
}

impl DynasmLabelApi for Assembler {
    /// tuple of encoded (type_,)
    type Relocation = (u8,);

    #[inline]
    fn registry(&self) -> &LabelRegistry {
        &self.labels
    }

    #[inline]
    fn registry_mut(&mut self) -> &mut LabelRegistry {
        &mut self.labels
    }

    #[inline]
    fn local_label(&mut self, name: &'static str) {
        let offset = self.offset();
        if let Some(relocs) = self.local_relocs.remove(&name) {
            for loc in relocs {
                self.patch_loc(loc, offset.0);
            }
        }
        self.labels.define_local(name, offset);
    }

    #[inline]
    fn global_reloc(&mut self, name: &'static str, kind: Self::Relocation) {
        let offset = self.offset().0;
        self.global_relocs.push((PatchLoc(offset, RelocationType::from_tuple(kind)), name));
    }

    #[inline]
    fn dynamic_reloc(&mut self, id: DynamicLabel, kind: Self::Relocation) {
        let offset = self.offset().0;
        self.dynamic_relocs.push((PatchLoc(offset, RelocationType::from_tuple(kind)), id));
    }

    #[inline]
    fn forward_reloc(&mut self, name: &'static str, kind: Self::Relocation) {
        let offset = self.offset().0;
        match self.local_relocs.entry(name) {
            Occupied(mut o) => {
                o.get_mut().push(PatchLoc(offset, RelocationType::from_tuple(kind)));
            },
            Vacant(v) => {
                v.insert(vec![PatchLoc(offset, RelocationType::from_tuple(kind))]);
            }
        }
    }

    #[inline]
    fn backward_reloc(&mut self, name: &'static str, kind: Self::Relocation) {
        let target = self.labels.resolve_local(name).unwrap();
        let offset = self.offset().0;
        self.patch_loc(PatchLoc(
            offset,
            RelocationType::from_tuple(kind)
        ), target.0)
    }

    #[inline]
    fn bare_reloc(&mut self, target: usize, kind: Self::Relocation) {
        let offset = self.offset().0;
        self.patch_loc(PatchLoc(
            offset,
            RelocationType::from_tuple(kind)
        ), target);
    }
}

impl Extend<u8> for Assembler {
    #[inline]
    fn extend<T>(&mut self, iter: T) where T: IntoIterator<Item=u8> {
        self.base.extend(iter)
    }
}

impl<'a> Extend<&'a u8> for Assembler {
    #[inline]
    fn extend<T>(&mut self, iter: T) where T: IntoIterator<Item=&'a u8> {
        self.base.extend(iter)
    }
}


/// This struct is a wrapper around an `Assembler` normally created using the
/// `Assembler.alter` method. Instead of writing to a temporary assembling buffer,
/// this struct assembles directly into an executable buffer. The `goto` method can
/// be used to set the assembling offset in the `ExecutableBuffer` of the assembler
/// (this offset is initialized to 0) after which the data at this location can be
/// overwritten by assembling into this struct.
pub struct AssemblyModifier<'a: 'b, 'b> {
    assembler: &'a mut Assembler,
    buffer: &'b mut MutableBuffer,
    asmoffset: usize
}

impl<'a, 'b> AssemblyModifier<'a, 'b> {
    /// Sets the current modification offset to the given value
    #[inline]
    pub fn goto(&mut self, offset: AssemblyOffset) {
        self.asmoffset = offset.0;
    }

    /// Checks that the current modification offset is not larger than the specified offset.
    #[inline]
    pub fn check(&mut self, offset: AssemblyOffset) -> Result<(), DynasmError> {
        if self.asmoffset > offset.0 {
            Err(DynasmError::CheckFailed)
        } else {
            Ok(())
        }
    }

    /// Checks that the current modification offset is exactly the specified offset.
    #[inline]
    pub fn check_exact(&mut self, offset: AssemblyOffset) -> Result<(), DynasmError> {
        if self.asmoffset != offset.0 {
            Err(DynasmError::CheckFailed)
        } else {
            Ok(())
        }
    }

    #[inline]
    fn patch_loc(&mut self, loc: PatchLoc, target: usize) {
        // calculate the offset that the relocation starts at
        // in the executable buffer
        let offset = loc.0 - loc.1.size();

        // the value that the relocation will have
        let t = target.wrapping_sub(loc.0 as usize);

        // write the relocation
        let buf = &mut self.buffer[offset .. offset + loc.1.size()];

        // handle non-bitfield variants
        let mask = match loc.1 {
            RelocationType::LITERAL32 => {
                LittleEndian::write_u32(buf, t as u32);
                return;
            },
            RelocationType::LITERAL64 => {
                LittleEndian::write_u64(buf, t as u64);
                return;
            },
            RelocationType::B => 0xFC000000,
            RelocationType::BCOND => 0xFF00001F,
            RelocationType::ADR => 0x9F00001F,
            RelocationType::ADRP => 0x9F00001F,
            RelocationType::TBZ => 0xFFF8001F,
        };

        let base = LittleEndian::read_u32(buf) & mask;
        let t = t as u32;

        let patch = match loc.1 {
            RelocationType::B => (t >> 2) & 0x3FFFFFF,
            RelocationType::BCOND => ((t >> 2) & 0x7FFFF) << 5,
            RelocationType::ADR => ((t & 0x3) << 29) | (((t >> 2) & 0x7FFFF) << 5),
            RelocationType::ADRP => (((t >> 12) & 0x3) << 29) | (((t >> 14) & 0x7FFFF) << 5),
            RelocationType::TBZ => ((t >> 2) & 0x3FFF) << 5,
            RelocationType::LITERAL32 |
            RelocationType::LITERAL64 => unreachable!(),
        };

        LittleEndian::write_u32(buf, base | (patch & !mask));
    }

    fn encode_relocs(&mut self) {
        let mut relocs = Vec::new();
        mem::swap(&mut relocs, &mut self.assembler.global_relocs);
        for (loc, name) in relocs {
            let target = self.assembler.labels.resolve_global(name).unwrap();
            self.patch_loc(loc, target.0);
        }

        let mut relocs = Vec::new();
        mem::swap(&mut relocs, &mut self.assembler.dynamic_relocs);
        for (loc, id) in relocs {
            let target = self.assembler.labels.resolve_dynamic(id).unwrap();
            self.patch_loc(loc, target.0);
        }

        if let Some(name) = self.assembler.local_relocs.keys().next() {
            panic!("Unknown local label '{}'", name);
        }
    }
}

impl<'a, 'b> DynasmApi for AssemblyModifier<'a, 'b> {
    #[inline]
    fn offset(&self) -> AssemblyOffset {
        AssemblyOffset(self.asmoffset)
    }

    #[inline]
    fn push(&mut self, value: u8) {
        self.buffer[self.asmoffset] = value;
        self.asmoffset += 1;
    }

    #[inline]
    fn align(&mut self, alignment: usize) {
        self.assembler.align(alignment);
    }
}

impl<'a, 'b> DynasmLabelApi for AssemblyModifier<'a, 'b> {
    type Relocation = (u8,);

    #[inline]
    fn registry(&self) -> &LabelRegistry {
        &self.assembler.labels
    }

    #[inline]
    fn registry_mut(&mut self) -> &mut LabelRegistry {
        &mut self.assembler.labels
    }

    #[inline]
    fn global_label(&mut self, name: &'static str) {
        self.assembler.global_label(name);
    }

    #[inline]
    fn global_reloc(&mut self, name: &'static str, kind: Self::Relocation) {
        let offset = self.asmoffset;
        self.assembler.global_relocs.push((PatchLoc(offset, RelocationType::from_tuple(kind)), name));
    }

    #[inline]
    fn dynamic_label(&mut self, id: DynamicLabel) {
        self.assembler.dynamic_label(id);
    }

    #[inline]
    fn dynamic_reloc(&mut self, id: DynamicLabel, kind: Self::Relocation) {
        let offset = self.asmoffset;
        self.assembler.dynamic_relocs.push((PatchLoc(offset, RelocationType::from_tuple(kind)), id));
    }

    #[inline]
    fn local_label(&mut self, name: &'static str) {
        let offset = self.offset();
        if let Some(relocs) = self.assembler.local_relocs.remove(&name) {
            for loc in relocs {
                self.patch_loc(loc, offset.0);
            }
        }
        self.assembler.labels.define_local(name, offset);
    }

    #[inline]
    fn forward_reloc(&mut self, name: &'static str, kind: Self::Relocation) {
        let offset = self.asmoffset;
        match self.assembler.local_relocs.entry(name) {
            Occupied(mut o) => {
                o.get_mut().push(PatchLoc(offset, RelocationType::from_tuple(kind)));
            },
            Vacant(v) => {
                v.insert(vec![PatchLoc(offset, RelocationType::from_tuple(kind))]);
            }
        }
    }

    #[inline]
    fn backward_reloc(&mut self, name: &'static str, kind: Self::Relocation) {
        let target = self.assembler.labels.resolve_local(name).unwrap();
        let offset = self.offset();
        self.patch_loc(PatchLoc(
            offset.0,
            RelocationType::from_tuple(kind)
        ), target.0)
    }

    #[inline]
    fn bare_reloc(&mut self, target: usize, kind: Self::Relocation) {
        let offset = self.offset().0;
        self.patch_loc(PatchLoc(
            offset,
            RelocationType::from_tuple(kind)
        ), target);
    }
}

impl<'a, 'b> Extend<u8> for AssemblyModifier<'a, 'b> {
    #[inline]
    fn extend<T>(&mut self, iter: T) where T: IntoIterator<Item=u8> {
        for i in iter {
            self.push(i)
        }
    }
}

impl<'a, 'b, 'c> Extend<&'c u8> for AssemblyModifier<'a, 'b> {
    #[inline]
    fn extend<T>(&mut self, iter: T) where T: IntoIterator<Item=&'c u8> {
        self.extend(iter.into_iter().cloned())
    }
}

/// Helper function for validating that a given value can be encoded as a 32-bit logical immediate
pub fn encode_logical_immediate_32bit(value: u32) -> Option<u16> {
    let transitions = value ^ value.rotate_right(1);
    let element_size = (64u32).checked_div(transitions.count_ones())?;

    // confirm that the elements are identical
    if value != value.rotate_left(element_size) {
        return None;
    }

    let element = value & (1u32 << element_size).wrapping_sub(1);
    let ones = element.count_ones();
    let imms = (!((element_size << 1) - 1) & 0x3F) | (ones - 1);

    let immr = if (element & 1) != 0 {
        ones - (!element).trailing_zeros()
    } else {
        element_size - element.trailing_zeros()
    };

    Some(((immr as u16) << 6) | (imms as u16))
}

/// Helper function for validating that a given value can be encoded as a 64-bit logical immediate
pub fn encode_logical_immediate_64bit(value: u64) -> Option<u16> {
    let transitions = value ^ value.rotate_right(1);
    let element_size = (128u32).checked_div(transitions.count_ones())?;

    // confirm that the elements are identical
    if value != value.rotate_left(element_size) {
        return None;
    }

    let element = value & (1u64 << element_size).wrapping_sub(1);
    let ones = element.count_ones();
    let imms = (!((element_size << 1) - 1) & 0x7F) | (ones - 1);

    let immr = if (element & 1) != 0 {
        ones - (!element).trailing_zeros()
    } else {
        element_size - element.trailing_zeros()
    };

    let n = imms & 0x40 == 0;
    let imms = imms & 0x3F;

    Some(((n as u16) << 12) | ((immr as u16) << 6) | (imms as u16))
}

/// Helper function for validating that a given value can be encoded as a floating point immediate
pub fn encode_floating_point_immediate(value: f32) -> Option<u8> {
    // floating point ARM immediates are encoded as
    // abcdefgh => aBbbbbbc defgh000 00000000 00000000
    // where B = !b
    // which means we can just slice out "a" and "bcdefgh" and assume the rest was correct

    let bits = value.to_bits();

    let check = (bits >> 25) & 0x3F;
    if (check == 0b100000 || check == 0b011111) && (bits & 0x7FFFF) == 0 {
        Some((((bits >> 24) & 0x80) | ((bits >> 19) & 0x7F)) as u8)
    } else {
        None
    }
}