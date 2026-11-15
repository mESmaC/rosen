- Each command runs through the platform shell (`cmd /C` on Windows, `sh -c` elsewhere) from the workspace root, and stops the prepare at its first failure.
  A command still running after an hour counts as failed.
