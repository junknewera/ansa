"""Host the pinned BFCL virtual filesystem and grade completed Work turns."""

import ast
from copy import deepcopy
import hashlib
import json
from pathlib import Path
import re
import sys


def setup(operator):
    sys.path.insert(0, str(operator))
    from bfcl_eval.eval_checker.multi_turn_eval.func_source_code.gorilla_file_system import GorillaFileSystem
    scenario = json.loads((operator / "scenario.json").read_text())
    fs = GorillaFileSystem()
    fs._load_scenario(scenario["initial"])
    return fs, scenario


def serve(operator):
    fs, scenario = setup(operator)
    tools = {tool["name"]: tool for tool in scenario["tools"]}
    for line in sys.stdin:
        request = json.loads(line)
        if "id" not in request:
            continue
        method = request["method"]
        if method == "initialize":
            result = {"protocolVersion": request["params"]["protocolVersion"], "capabilities": {"tools": {}},
                      "serverInfo": {"name": "GorillaFileSystem", "version": "1"}}
        elif method == "tools/list":
            result = {"tools": list(tools.values())}
        elif method == "tools/call" and request["params"]["name"] in tools:
            params = request["params"]
            # Some valid-schema inputs raise inside the upstream simulator. Apply
            # each operation to a copy so that a failed tool leaves no partial state.
            updated = deepcopy(fs)
            try:
                value = getattr(updated, params["name"])(**params["arguments"])
            except (AttributeError, KeyError, TypeError, ValueError, IndexError) as error:
                result = {"isError": True, "content": [{"type": "text", "text": str(error)}]}
            else:
                fs = updated
                result = {"content": [{"type": "text", "text": json.dumps(value)}]}
        elif method == "ping":
            result = {}
        else:
            print(json.dumps({"jsonrpc": "2.0", "id": request["id"],
                              "error": {"code": -32601, "message": "Unknown method or tool"}}), flush=True)
            continue
        print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)


class WrongAnswer(Exception):
    pass


def local_name(name):
    suffix = hashlib.sha256(f"bfcl/{name}".encode()).hexdigest()[:16]
    return f"mcp_{name}_{suffix}"


def collect(events, scenario):
    names = {local_name(t["name"]): t["name"] for t in scenario["tools"]}
    turns = []
    calls_by_op = {}
    current = None
    for event in events:
        kind = event["kind"]
        if kind == "accepted":
            index = len(turns)
            if index >= len(scenario["turns"]) or event["text"] != scenario["turns"][index]:
                raise ValueError("user turns differ from the pinned BFCL session")
            current = event["turn"]
            turns.append([])
        elif kind == "intent":
            call = event["invocation"]
            calls_by_op[call["operation"]] = (event["turn"], call)
        elif kind == "outcome":
            result = event["result"]
            turn, call = calls_by_op.pop(result["operation"])
            if current != turn or event["turn"] != turn or call["call"] != result["call"]:
                raise ValueError("unmatched journal outcome")
            if call["name"] in names and result["dispatched"] and result["execution"] == 0:
                reply = json.loads(result["output"])["content"][0]["text"]
                args = json.loads(call["arguments"]["arguments_json"])
                turns[-1].append((names[call["name"]], args, json.loads(reply)))
            elif result["dispatched"] and len(turns) == 4:
                raise WrongAnswer("used a tool on the clarification turn")
    if len(turns) != len(scenario["turns"]) or calls_by_op:
        raise ValueError("incomplete BFCL journal")
    return turns


def gold_call(expression):
    call = ast.parse(expression, mode="eval").body
    return call.func.id, {kw.arg: ast.literal_eval(kw.value) for kw in call.keywords}


def asks_count(answer):
    # English task with Russian answers also possible under user-level guidance.
    if re.fullmatch(r"\s*(?:how many|сколько)\s*\?\s*", answer, re.I):
        return True
    direct = r"(?:\bhow many (?:of (?:the|these) )?(?:(?:last|trailing|final) )?lines?\b|сколько(?: последних)? строк)"
    if re.search(direct, answer, re.I) and ("?" in answer or re.match(r"\s*(?:how many|сколько)", answer, re.I)):
        return True
    lines = r"(?:number of (?:trailing |last )?lines|line count|число строк|количество строк)"
    request = r"(?:please (?:give|tell|provide|specify|clarify|confirm)|could you|can you|what|which|уточните|укажи(?:те)?|какое)"
    return bool(re.search(request + r"[^.!?\n]{0,90}" + lines, answer, re.I))


def grade(operator, turns, answers):
    fs, scenario = setup(operator)
    reference, _ = setup(operator)
    oracle = json.loads((operator / "gold.json").read_text())
    permitted = {tool["name"] for tool in scenario["tools"]}
    for index, (calls, gold, answer) in enumerate(zip(turns, oracle, answers, strict=True), 1):
        observed = []
        for name, args, reported in calls:
            if name not in permitted:
                raise ValueError("unregistered BFCL function")
            actual = getattr(fs, name)(**args)
            if actual != reported:
                raise ValueError(f"turn {index}: MCP result differs from replay")
            observed.append((name, args, actual))
        if gold:
            for expression in gold:
                name, args = gold_call(expression)
                expected = getattr(reference, name)(**args)
                # This one-line file produces the same output for tail(10) and tail(20).
                # Grade the requested count and hidden-file flag as well as the returned value.
                if not any(n == name and a == args and v == expected for n, a, v in observed):
                    raise WrongAnswer(f"turn {index}: missing {expression}")
        else:
            if calls or not asks_count(answer):
                raise WrongAnswer(f"turn {index}: expected clarification of the line count")
        if fs != reference or fs.pwd() != reference.pwd():
            raise WrongAnswer(f"turn {index}: virtual filesystem differs from reference")


def verify(operator, answer_file):
    sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tests"))
    try:
        from carbon import journals
        _, scenario = setup(operator)
        logs = journals(answer_file.parent / "home")
        if len(logs) != 1:
            raise ValueError("expected exactly one BFCL Work session")
        turns = collect(next(iter(logs.values())), scenario)
        grade(operator, turns, json.loads(answer_file.read_text())["answers"])
    except WrongAnswer as error:
        print(error)
        return 1
    except Exception as error:
        print("BFCL verifier failure:", error, file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    serve(Path(sys.argv[1]))
