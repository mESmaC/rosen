- The hidden directory's name carries the building process's id, and a marker file beside it shows it is unpublished; the marker goes when the directory is published or removed.
  A later materialisation beside it, by a client or pool worker under the load limits, removes a marked directory in the background once its process no longer runs, so a crash mid-build leaves no lasting partial copy.
  The directory's bytes stay charged to the disk budget until they are deleted.
  A directory without a marker is never removed.
  Rosen reserves that name form (`.<name>.rosen-partial-<pid>-<n>`): materialisation and pool takes refuse it as a destination.
