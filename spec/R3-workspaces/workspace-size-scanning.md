- One process per machine at a time scans workspace files, outside the admission lock, so admissions and pool takes never wait behind a directory walk.
  The next scan starts a second after the last, or after as long as that scan took if longer.
  A scan starts walks for at most 200 ms, workspaces measured longest ago first, so the request that runs it waits about that long plus one workspace's walk, and the scans cycle through every workspace in turn.
  Admission charges each workspace its last measured size, and a reservation no scan has measured yet its reserved bytes.
  A scan drops records of deleted workspaces, so the registry stays as large as the workspaces on disk.
  Blob sizes for a reservation come from object headers, without reading the blobs.
