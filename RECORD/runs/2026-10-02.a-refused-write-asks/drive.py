import json, os, shutil, subprocess, sys, tempfile, time
answer, seed, out = sys.argv[1], sys.argv[2], os.path.abspath(sys.argv[3])
root = "/Users/JG31772/dev/joshua/luu"
work = tempfile.mkdtemp(prefix="ask-7b-")
home = tempfile.mkdtemp(prefix="ask-7b-home-")
open(os.path.join(work, "README.md"), "w").write("# demo\n\nA scratch project.\n")
cmd = [f"{root}/target/release/luu", "stdio", "--allow-write", ".", "--no-store",
       "--backend", "openai", "--openai-url", "http://127.0.0.1:8080/v1",
       "--model", "qwen2.5-coder:7b", "--tool-calls", "native",
       "--context-limit", "32768", "--temperature", "0", "--seed", seed,
       "--record", out]
p = subprocess.Popen(cmd, cwd=work, env={**os.environ, "LUU_HOME": home},
                     stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, bufsize=1)
def send(m): p.stdin.write(json.dumps(m) + "\n"); p.stdin.flush()
send({"type": "prompt", "text": "crea un fichero ./example.html en el proyecto"})
text, t0 = "", time.time()
for line in p.stdout:
    m = json.loads(line); k = m["type"]
    if k == "token": text += m["text"]; continue
    if k == "tool_call": print(f"  tool_call   step {m['step']}: {m['name']} {json.dumps(m['arguments'], ensure_ascii=False)[:160]}")
    elif k == "call_held":
        print(f"  call_held   step {m['step']} -> answering allow={answer=='allow'}")
        send({"type": "answer_call", "turn": m["turn"], "step": m["step"], "allow": answer == "allow"})
    elif k == "tool_result":
        print(f"  tool_result step {m['step']}: allowed={m['verdict']['allowed']} asked={m.get('asked')} error={m.get('error')}")
    elif k in ("ended", "failed"):
        print(f"  {k}: {m.get('reason') or m.get('message')}  ({time.time()-t0:.1f}s)")
        break
p.terminate()
print("  final text:", text.strip()[-600:].replace("\n", "\n    "))
f = os.path.join(work, "example.html")
print("  example.html:", repr(open(f).read()) if os.path.exists(f) else "absent")
shutil.rmtree(work); shutil.rmtree(home)
