; Markdown uses the block grammar in Zed, which does not expose inline math nodes.
; Math previews are defined on the markdown-inline language instead.
; Keep a valid no-op query so loading succeeds without math captures.
((thematic_break) @_ignore
 (#match? @_ignore "^$"))
