; Symbol tags for MoonBit.
; The vendored grammar ships no tags.scm, so octorus bundles its own.

(function_definition (function_identifier (lowercase_identifier) @name)) @definition.function

(struct_definition (identifier) @name) @definition.class
(enum_definition (identifier) @name) @definition.class
(trait_definition (identifier) @name) @definition.interface
(type_definition (identifier) @name) @definition.type
