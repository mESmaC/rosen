- At most two requests' deferred post-land jobs run concurrently.
  Waiting jobs occupy no blocking worker, leaving capacity for new land requests.
  Refused requests schedule no deferred stack refresh.
  The synchronous stack phase and durable pending-work recovery rules still apply.
