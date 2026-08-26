;; Kotlin highlights for the tree-sitter-kotlin-ng grammar (v1.1).
;;
;; IMPORTANT: Same-node captures resolve "later pattern wins" (the
;; highlighter iterates with `captures()`, which yields in pattern order).
;; The generic `(identifier) @variable` catch-all therefore comes FIRST and
;; every specific pattern below overrides it.

(identifier) @variable

;; Uppercase identifiers as types/classes; overridden by the specific
;; function/property rules below.
((identifier) @type
 (#match? @type "^[A-Z]"))

;;; Comments

[
	(line_comment)
	(block_comment)
] @comment

;;; Literals

(number_literal) @number
(float_literal) @number

(character_literal) @string

(string_literal) @string
(multiline_string_literal) @string
(escape_sequence) @string.escape

(string_literal (interpolation) @embedded)
(multiline_string_literal (interpolation) @embedded)

;; true/false/null are parsed as plain identifiers by this grammar
((identifier) @boolean
 (#any-of? @boolean "true" "false"))
((identifier) @constant
 (#eq? @constant "null"))

;;; Types

(user_type (identifier) @type)

((user_type (identifier) @type.builtin)
 (#any-of? @type.builtin
   "Int" "Long" "Short" "Byte" "Float" "Double" "Boolean" "Char" "String"
   "Unit" "Any" "Nothing" "Array" "List" "Map" "Set"))

;;; Functions

;; Function declarations: `fun name(...)`
(function_declaration (identifier) @function)

;; Function calls with a bare callee: `name(...)`
(call_expression (identifier) @function)

;; Method calls: `obj.name(...)` / `a.b.name(...)`
(call_expression
	(navigation_expression (identifier) "." (identifier) @function))
(call_expression
	(navigation_expression (navigation_expression) "." (identifier) @function))

;; Constructor-style calls (uppercase callee): `Person(...)`
((call_expression (identifier) @constructor)
 (#match? @constructor "^[A-Z]"))

;; Built-in top-level functions
((identifier) @function.builtin
 (#any-of? @function.builtin "println" "print" "error" "require" "check" "TODO"))

;;; Properties / parameters

;; Property declarations: `val/var name`
(property_declaration
	(variable_declaration (identifier) @property))

;; Class properties (primary constructor): `class Foo(val name: String)`
(class_parameter (identifier) @property)

;; Function parameters
(parameter (identifier) @variable.parameter)

;; Named call arguments: `f(x = 1)`
(value_argument (identifier) @property "=")

;; Field access: `obj.field` (last segment only; the receiver keeps its own style)
(navigation_expression (identifier) "." (identifier) @property)

;;; Class-like declarations

(class_declaration (identifier) @type)
(object_declaration (identifier) @type)
(type_alias (identifier) @type)
(enum_entry (identifier) @variant)

;;; Modifiers & keywords

[
	"package"
	"import"
	"class"
	"object"
	"interface"
	"enum"
	"fun"
	"val"
	"var"
	"when"
	"if"
	"else"
	"for"
	"while"
	"do"
	"try"
	"catch"
	"finally"
	"throw"
	"return"
	"is"
	"in"
	"as"
	"by"
	"where"
	"init"
	"constructor"
	"companion"
	"override"
	"open"
	"abstract"
	"final"
	"private"
	"protected"
	"internal"
	"public"
	"suspend"
	"inline"
	"noinline"
	"crossinline"
	"operator"
	"infix"
	"external"
	"lateinit"
	"const"
	"vararg"
	"out"
	"typealias"
	"get"
	"set"
] @keyword

(visibility_modifier) @keyword
(inheritance_modifier) @keyword
(member_modifier) @keyword
(function_modifier) @keyword
(property_modifier) @keyword
(platform_modifier) @keyword
(variance_modifier) @keyword
(parameter_modifier) @keyword
(reification_modifier) @keyword
(class_modifier) @keyword
(type_modifiers) @keyword

(this_expression) @variable.special
(super_expression) @variable.special

;; `field` inside property accessors and `it` inside lambdas
((identifier) @variable.special
 (#eq? @variable.special "field"))
((identifier) @variable.special
 (#eq? @variable.special "it"))

(label) @label

;;; Annotations

(annotation "@" @punctuation.special)
(annotation (user_type (identifier) @attribute))

;;; Operators & punctuation

[
	"+"
	"-"
	"*"
	"/"
	"%"
	"="
	"=="
	"==="
	"!="
	"!=="
	"<"
	">"
	"<="
	">="
	"&&"
	"||"
	"!"
	"!!"
	"++"
	"--"
	"+="
	"-="
	"*="
	"/="
	"%="
	".."
	"..<"
	"?."
	"?:"
	"::"
	"->"
	"?"
	"&"
] @operator

[
	"("
	")"
	"["
	"]"
	"{"
	"}"
] @punctuation.bracket

[
	","
	"."
	";"
	":"
] @punctuation.delimiter
