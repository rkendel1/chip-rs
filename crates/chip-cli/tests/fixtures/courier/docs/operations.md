# Operations notes

* **Capping sleeps.** Operators set `[client] max_wait` to guarantee the client never sleeps longer
  than a known bound, whatever the server asks for. A wait over the bound fails the request.
* **Circuit state** is saved beside the journal after every request and reloaded on start, so a
  route that was open stays open across a restart.
* **Journal lines** that fail to parse are skipped and counted by `courier journal stats`; they are
  never repaired.
* **Environment overrides** are the supported way to change a single key per deployment.
