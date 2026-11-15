- A requester whose connection drops mid-land asks again on a fresh connection, starting the server if it is down.
  A repeat is answered `already_landed` with the landed commit, so a server crash never loses the answer or doubles the land.
