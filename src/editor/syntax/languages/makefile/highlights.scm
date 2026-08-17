; Makefile highlights for DeepTerm.
;
; Based on tree-sitter-make's highlights.scm, adjusted for this app's
; SimpleHighlighter:
; - `#match?` predicates are NOT evaluated (plain QueryCursor::matches), so
;   predicate-gated rules would match every node; they have been removed and
;   replaced with their general (unguarded) form.
; - `@clean` (Zed-specific) captures removed.

[
 "("
 ")"
 "{"
 "}"
] @punctuation.bracket

[
 ":"
 "&:"
 "::"
 "|"
 ";"
 "\""
 "'"
 ","
] @punctuation.delimiter

[
 "$"
 "$$"
] @punctuation.special

(automatic_variable
 [ "@" "%" "<" "?" "^" "+" "/" "*" "D" "F"] @punctuation.special)

[
 "="
 ":="
 "::="
 "?="
 "+="
 "!="
 "@"
 "-"
 "+"
] @operator

[
 (text)
 (string)
 (raw_text)
] @string

(variable_assignment (word) @string)

[
 "ifeq"
 "ifneq"
 "ifdef"
 "ifndef"
 "else"
 "endif"
 "if"
 "or"  ; boolean functions are conditional in make grammar
 "and"
] @conditional

"foreach" @repeat

[
 "define"
 "endef"
 "vpath"
 "undefine"
 "export"
 "unexport"
 "override"
 "private"
] @keyword

[
 "include"
 "sinclude"
 "-include"
] @include

[
 "subst"
 "patsubst"
 "strip"
 "findstring"
 "filter"
 "filter-out"
 "sort"
 "word"
 "words"
 "wordlist"
 "firstword"
 "lastword"
 "dir"
 "notdir"
 "suffix"
 "basename"
 "addsuffix"
 "addprefix"
 "join"
 "wildcard"
 "realpath"
 "abspath"
 "call"
 "eval"
 "file"
 "value"
 "shell"
] @keyword.function

[
 "error"
 "warning"
 "info"
] @exception

;; Variables
(variable_assignment
  name: (word) @constant)

(variable_reference
  (word) @constant)

(comment) @comment

;; Well-known implicit-rule variables / special variables
[
 "VPATH"
 ".RECIPEPREFIX"
] @constant.builtin

;; Targets (including standard/builtin targets)
(targets
  (word) @constant.macro)