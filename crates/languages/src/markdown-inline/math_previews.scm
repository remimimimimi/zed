; $...$        => LaTeX inline
; $$...$$      => LaTeX block
; \(...\)      => LaTeX inline
; \[...\]      => LaTeX block
; $...$        => Typst inline (backend alternative)
; $$...$$      => Typst block (backend alternative)

((latex_block
  (latex_span_delimiter) @_open
  (_)*
  (latex_span_delimiter) @_close) @math.inline.latex
 (#match? @_open "^\\$$")
 (#match? @_close "^\\$$")
 (#offset! @math.inline.latex 0 1 0 -1))

((latex_block
  (latex_span_delimiter) @_open
  (_)*
  (latex_span_delimiter) @_close) @math.block.latex
 (#match? @_open "^\\$\\$$")
 (#match? @_close "^\\$\\$$")
 (#offset! @math.block.latex 0 2 0 -2))

((latex_block
  (latex_span_delimiter) @_open
  (_)*
  (latex_span_delimiter) @_close) @math.inline.typst
 (#match? @_open "^\\$$")
 (#match? @_close "^\\$$")
 (#offset! @math.inline.typst 0 1 0 -1))

((latex_block
  (latex_span_delimiter) @_open
  (_)*
  (latex_span_delimiter) @_close) @math.block.typst
 (#match? @_open "^\\$\\$$")
 (#match? @_close "^\\$\\$$")
 (#offset! @math.block.typst 0 2 0 -2))

((inline
  (backslash_escape) @math.inline.latex @_open
  (_)*
  (backslash_escape) @_close)
 (#match? @_open "^\\\\\\($")
 (#match? @_close "^\\\\\\)$")
 (#make-range! @math.inline.latex @_open @_close)
 (#offset! @math.inline.latex 0 2 0 -2))

((inline
  (backslash_escape) @math.block.latex @_open
  (_)*
  (backslash_escape) @_close)
 (#match? @_open "^\\\\\\[$")
 (#match? @_close "^\\\\\\]$")
 (#make-range! @math.block.latex @_open @_close)
 (#offset! @math.block.latex 0 2 0 -2))
