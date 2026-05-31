"""ISTQB-style integration test suite for Event Horizon MCP server.
Tests execute against the Rust binary via stdio subprocess."""
import subprocess, json, time, sys, os

REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
FIXTURES = os.path.join(REPO, "tests", "fixtures")
BIN = os.path.join(REPO, "target", "release", "stria")
passed = 0
failed = 0

def test(name, fn):
    global passed, failed
    try:
        fn()
        passed += 1
        print(f"  PASS  {name}")
    except Exception as e:
        failed += 1
        print(f"  FAIL  {name}: {e}")

def serve(repo_path):
    proc = subprocess.Popen(
        [BIN, "serve", "--repo", repo_path],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True
    )
    time.sleep(0.8)
    def call(method, params=None):
        req = {"jsonrpc":"2.0","id":1,"method":method}
        if params: req["params"] = params
        proc.stdin.write(json.dumps(req)+chr(10))
        proc.stdin.flush(); time.sleep(0.3)
        return json.loads(proc.stdout.readline().strip())
    return proc, call

def main():
    # T1: health
    proc, call = serve(REPO)
    try:
        call("initialize", {"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"1"}})
        
        def tools_list():
            r = call("tools/list")
            return [t["name"] for t in r["result"]["tools"]]
        
        def tool(name, args=None):
            r = call("tools/call", {"name": name, "arguments": args or {}})
            return json.loads(r["result"]["content"][0]["text"])
        
        # T1: health
        test("health returns ok", lambda: (
            lambda h: (
                None if h.get("ok") == True and h.get("phrases", 0) > 0
                else (_ for _ in ()).throw(AssertionError(f"health bad: {h}"))
            )
        )(tool("health")))
        
        # T2: all 9 tools registered
        test("all tools registered", lambda: (
            lambda names: (
                None if all(t in names for t in 
                    ["orient","code_search","search","pre_edit",
                     "who_calls","hidden_deps","expand_body","find_hash","health"])
                else (_ for _ in ()).throw(AssertionError(f"missing tools: {names}"))
            )
        )(tools_list()))
        
        # T3: search returns results
        test("search finds files", lambda: (
            lambda r: (
                None if len(r.get("candidates", [])) > 0
                else (_ for _ in ()).throw(AssertionError(f"empty search: {r}"))
            )
        )(tool("search", {"query": "SpoolManager"})))
        
        # T4: search empty query returns empty
        test("search empty query", lambda: (
            lambda r: (
                None if len(r.get("candidates", [])) == 0
                else (_ for _ in ()).throw(AssertionError(f"expected empty: {r}"))
            )
        )(tool("search", {"query": ""})))
        
        # T5: pre_edit returns plan
        test("pre_edit returns plan", lambda: (
            lambda p: (
                None if p.get("edit") and p.get("verify") is not None
                else (_ for _ in ()).throw(AssertionError(f"bad plan: {p}"))
            )
        )(tool("pre_edit", {"task": "spool upload retry"})))
        
        # T6: code_search default tier
        test("code_search returns files", lambda: (
            lambda r: (
                None if len(r.get("files", [])) >= 1
                else (_ for _ in ()).throw(AssertionError(f"bad search: {r}"))
            )
        )(tool("code_search", {"task": "spool upload retry"})))
        
        # T7: code_search with expand_plan
        test("code_search +expand_plan has fixtures", lambda: (
            lambda r: (
                None if "fixtures" in r
                else (_ for _ in ()).throw(AssertionError(f"missing fixtures: {r}"))
            )
        )(tool("code_search", {"task": "spool upload retry", "expand_plan": True})))
        
        # T8: expand_body with unknown hash returns empty
        test("expand_body unknown hash", lambda: (
            lambda r: (
                None if r.get("body") == ""
                else (_ for _ in ()).throw(AssertionError(f"expected empty: {r}"))
            )
        )(tool("expand_body", {"hash": "nonexistent1234567890abcdef"})))
        
        # T9: find_hash returns results or empty
        test("find_hash doesn't crash", lambda: (
            tool("find_hash", {"name": "SpoolManager"})
        ))
        
        # T10: hidden_deps with nonexistent file returns empty
        test("hidden_deps nonexistent file", lambda: (
            lambda r: (
                None if r.get("deps") is not None
                else (_ for _ in ()).throw(AssertionError(f"bad deps: {r}"))
            )
        )(tool("hidden_deps", {"file": "nonexistent/file.go"})))
        
        # T11: orient returns manifest
        test("orient returns manifest", lambda: (
            lambda r: (
                None if r.get("schema_version") == 1 and r.get("n_files", 0) > 0
                else (_ for _ in ()).throw(AssertionError(f"bad orient: {r}"))
            )
        )(tool("orient")))
        
        # T12: who_calls with known identifier
        test("who_calls SpoolManager", lambda: (
            lambda r: (
                None if len(r.get("callers", [])) > 0
                else (_ for _ in ()).throw(AssertionError(f"no callers: {r}"))
            )
        )(tool("who_calls", {"name": "SpoolManager"})))
        
        # T13: unknown tool returns error
        test("unknown tool returns error", lambda: (
            lambda r: (
                None if "Unknown tool" in str(r.get("error", ""))
                else (_ for _ in ()).throw(AssertionError(f"expected error: {r}"))
            )
        )(tool("nonexistent_tool")))
        
        # T14: rapid-fire calls don't crash
        test("rapid-fire no crash", lambda: (
            [tool("health") for _ in range(10)] and None
        ))

        # T15: switch_repo changes the active repo
        test("switch_repo changes repo", lambda: (
            lambda r: (
                None if r.get("status") == "ok"
                else (_ for _ in ()).throw(AssertionError(f"switch_repo failed: {r}"))
            )
        )(tool("switch_repo", {"path": REPO})))

        # T16: orient after switch_repo shows different repo
        test("orient after switch_repo", lambda: (
            lambda r: (
                None if r.get("n_files", 0) > 0
                else (_ for _ in ()).throw(AssertionError(f"bad orient: {r}"))
            )
        )(tool("orient")))

        # T17: switch_repo with bad path returns error
        test("switch_repo bad path", lambda: (
            lambda r: (
                None if r.get("error") and "Path not found" in r["error"]
                else (_ for _ in ()).throw(AssertionError(f"expected error: {r}"))
            )
        )(tool("switch_repo", {"path": "/nonexistent/path"})))
        
    finally:
        proc.terminate()
    
    print(f"\n{'='*40}")
    print(f"Results: {passed} passed, {failed} failed, {passed+failed} total")
    return failed == 0

if __name__ == "__main__":
    sys.exit(0 if main() else 1)
