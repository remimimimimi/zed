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
