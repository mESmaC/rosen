- Each command runs through the platform shell (`cmd /C` on Windows, `sh -c` elsewhere) from the workspace root, and stops the prepare at its first failure.
  A command still running after an hour counts as failed.
  The worker drains command output while it runs and keeps only a bounded tail for the failure log.
