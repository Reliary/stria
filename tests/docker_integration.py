"""Docker integration test for stria."""
import subprocess, json, time, sys

proc = subprocess.Popen(
    ["stria", "serve", "--repo", "/test-repo"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True
)
time.sleep(0.8)

def call(name, args=None):
    req = {"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args or {}}}
    proc.stdin.write(json.dumps(req) + "\n")
    proc.stdin.flush()
    time.sleep(0.3)
    r = json.loads(proc.stdout.readline().strip())
    return json.loads(r["result"]["content"][0]["text"])

# Init
proc.stdin.write(json.dumps({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}) + "\n")
proc.stdin.flush()
time.sleep(0.3)
proc.stdout.readline()

# 1. Orient
o = call("orient")
assert o.get("n_files", 0) > 0, f"orient failed: {o}"
print(f"DOCKER OK: orient n_files={o['n_files']}")

# 2. Search
s = call("search", {"query": "hello"})
assert len(s.get("candidates", [])) > 0, f"search failed: {s}"
print(f"DOCKER OK: search {len(s['candidates'])} results")

# 3. Switch repo
r = call("switch_repo", {"path": "/test-repo"})
assert r.get("status") == "ok", f"switch_repo failed: {r}"
print(f"DOCKER OK: switch_repo")

# 4. Verify orient after switch
o2 = call("orient")
assert o2.get("n_files", 0) > 0, f"orient after switch failed: {o2}"
print(f"DOCKER OK: orient after switch n_files={o2['n_files']}")

# 5. Who calls
w = call("who_calls", {"name": "hello"})
assert len(w.get("callers", [])) > 0, f"who_calls failed: {w}"
print(f"DOCKER OK: who_calls {len(w['callers'])} callers")

proc.terminate()
print("\nAll Docker integration tests PASSED")
sys.exit(0)
