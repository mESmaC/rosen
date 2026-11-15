### R11 · History queries
- The server answers log, path history, blame, diff, merge base and file read at any commit or change through the API, with no checkout.
- It derives and caches the indexes these need from landed commits.
- **Accept when:** a reviewer can review a change and a checker can fetch its merge result without materialising a workspace.
