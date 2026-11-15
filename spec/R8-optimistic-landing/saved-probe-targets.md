- probe targets are saved before releasing the landed change's claims, so recovery reuses the same selection, including an empty selection.
  A selection that fails on what is stored, such as an unreadable project configuration at base, is logged and saved as empty.
  A busy database or I/O error is not saved: the post-land run fails and its replay selects again;
