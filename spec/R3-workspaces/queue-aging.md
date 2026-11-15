- Priority alone cannot hold a run forever.
  After its class's patience (`build` 3 minutes, `verify` 2, `prepare` 10) a waiter stops yielding to more important classes and needs a free slot of its own class and memory above the reserve.
  A stuck landing check cannot hold every agent's build, and steady agent builds cannot keep the tests after a land from running when memory permits.
