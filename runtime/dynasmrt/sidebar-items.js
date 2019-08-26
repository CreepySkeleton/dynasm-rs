initSidebarItems({"enum":[["DynasmError","An error type that is returned from various check and check_exact methods"]],"macro":[["MutPointer","Preforms the same action as the `Pointer!` macro, but casts to a *mut pointer."],["Pointer","This macro takes a *const pointer from the source operand, and then casts it to the desired return type. this allows it to be used as an easy shorthand for passing pointers as dynasm immediate arguments."]],"mod":[["aarch64",""],["common",""],["x64",""],["x86",""]],"struct":[["AssemblyOffset","A struct representing an offset into the assembling buffer of a `DynasmLabelApi` struct. The wrapped `usize` is the offset from the start of the assembling buffer in bytes."],["DynamicLabel","A dynamic label"],["ExecutableBuffer","A structure holding a buffer of executable memory"],["Executor","A read-only shared reference to the executable buffer inside an Assembler. By locking it the internal `ExecutableBuffer` can be accessed and executed."]],"trait":[["DynasmApi","This trait represents the interface that must be implemented to allow the dynasm preprocessor to assemble into a datastructure."],["DynasmLabelApi","This trait extends DynasmApi to not only allow assembling, but also labels and various directives"]]});