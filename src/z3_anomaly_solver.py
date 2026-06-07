import z3

def main():
    solver = z3.Solver()
    
    # 1. ADD jr_pub TO YOUR NODES ARRAY
    nodes = [
        "root",
        "alice_join",
        "jr_pub",       # <--- CRITICAL ADDITION
        "charlie_join",
        "bob_join",
        "pl_init",
        "ban_alice",
        "zombie_action",
        "merge",
    ]

    # 2. UPDATE TOPOLOGICAL EDGES
    edges = [
        ("root", "alice_join"),
        ("alice_join", "jr_pub"),       # <--- CRITICAL ADDITION
        ("jr_pub", "charlie_join"),     # <--- CRITICAL ADDITION
        ("charlie_join", "bob_join"),
        ("bob_join", "pl_init"),
        ("pl_init", "ban_alice"),
        ("pl_init", "zombie_action"),
        ("ban_alice", "merge"),
        ("zombie_action", "merge"),
    ]
    
    depth_vals = {node: z3.Int(f"depth_{node}") for node in nodes}
    ts_vals = {node: z3.Int(f"ts_{node}") for node in nodes}
    
    # Standard properties: depth >= 1, ts >= 1000
    for node in nodes:
        solver.add(depth_vals[node] >= 1)
        solver.add(ts_vals[node] >= 1000)
        
    # Edge constraints: depth of child > depth of parent, ts of child > ts of parent
    for parent, child in edges:
        solver.add(depth_vals[child] > depth_vals[parent])
        solver.add(ts_vals[child] > ts_vals[parent])
        
    # 3. ENFORCE STRICT TEMPORAL CHRONOLOGY FOR THE BASELINE
    # This prevents Z3 from assigning the same timestamp and scrambling Kahn's sort
    solver.add(ts_vals["root"] < ts_vals["alice_join"])
    solver.add(ts_vals["alice_join"] < ts_vals["jr_pub"])
    solver.add(ts_vals["jr_pub"] < ts_vals["charlie_join"])
    
    if solver.check() == z3.sat:
        model = solver.model()
        depth_resolved = {node: model[depth_vals[node]].as_long() for node in nodes}
        ts_resolved = {node: model[ts_vals[node]].as_long() for node in nodes}
        
        dsl_output = []
        
        # 4. ADD DSL OUTPUT LOGIC
        dsl_output.append(
            f'node root [type="m.room.create", '
            f'sender="@alice:ServerA", state_key="", depth={depth_resolved["root"]}, '
            f'origin_server_ts={ts_resolved["root"]}, content={{"room_version": "12"}}]'
        )
        dsl_output.append(
            f'node alice_join [type="m.room.member", '
            f'sender="@alice:ServerA", state_key="@alice:ServerA", depth={depth_resolved["alice_join"]}, '
            f'origin_server_ts={ts_resolved["alice_join"]}, content={{"membership": "join"}}]'
        )
        dsl_output.append(
            f'node jr_pub [type="m.room.join_rules", '
            f'sender="@alice:ServerA", state_key="", depth={depth_resolved["jr_pub"]}, '
            f'origin_server_ts={ts_resolved["jr_pub"]}, content={{"join_rule": "public"}}]'
        )
        dsl_output.append(
            f'node charlie_join [type="m.room.member", '
            f'sender="@charlie:ServerB", state_key="@charlie:ServerB", depth={depth_resolved["charlie_join"]}, '
            f'origin_server_ts={ts_resolved["charlie_join"]}, content={{"membership": "join"}}]'
        )
        dsl_output.append(
            f'node bob_join [type="m.room.member", '
            f'sender="@bob:ServerB", state_key="@bob:ServerB", depth={depth_resolved["bob_join"]}, '
            f'origin_server_ts={ts_resolved["bob_join"]}, content={{"membership": "join"}}]'
        )
        dsl_output.append(
            f'node pl_init [type="m.room.power_levels", '
            f'sender="@alice:ServerA", state_key="", depth={depth_resolved["pl_init"]}, '
            f'origin_server_ts={ts_resolved["pl_init"]}, content={{"users": {{"@alice:ServerA": 100, "@bob:ServerB": 50, "@charlie:ServerB": 50}}}}]'
        )
        dsl_output.append(
            f'node ban_alice [type="m.room.member", '
            f'sender="@bob:ServerB", state_key="@alice:ServerA", depth={depth_resolved["ban_alice"]}, '
            f'origin_server_ts={ts_resolved["ban_alice"]}, content={{"membership": "ban"}}]'
        )
        dsl_output.append(
            f'node zombie_action [type="m.room.name", '
            f'sender="@alice:ServerA", state_key="", depth={depth_resolved["zombie_action"]}, '
            f'origin_server_ts={ts_resolved["zombie_action"]}, content={{"name": "Zombie Land"}}]'
        )
        dsl_output.append(
            f'node merge [type="merge", '
            f'sender="", state_key="", depth={depth_resolved["merge"]}, '
            f'origin_server_ts={ts_resolved["merge"]}, content={{}}]'
        )
        
        print("\n".join(dsl_output))
    else:
        print("Unsatisfiable constraints")

if __name__ == "__main__":
    main()
