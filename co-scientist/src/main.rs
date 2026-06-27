//! Co-scientist CLI.
//!
//! Usage:
//!   co-scientist init                              -- create DB, seed agents, init skills dir
//!   co-scientist run --agent NAME -- "..."         -- run a single turn for an agent
//!   co-scientist memory count                      -- print row counts
//!   co-scientist memory recent [N]                 -- last N events
//!   co-scientist memory context --agent NAME -- "..." -- render a context block
//!   co-scientist tools list                        -- list registered tools
//!   co-scientist skills list [--dir PATH]          -- list discovered skills
//!   co-scientist enqueue --action NAME -- "JSON"   -- enqueue a task
//!   co-scientist serve                             -- run the worker loop until ctrl-c
//!   co-scientist promote                           -- run memory consolidation (stub)

use std::env;

use anyhow::{Context, Result};
use co_scientist::{
    db,
    db::Db,
    memory::{new_run_id, ContextLimits, Memory},
    runner::{Runner, RunnerConfig},
};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        usage();
        std::process::exit(2);
    }
    let cmd = &args[1];

    let db_path = env::var("CO_SCIENTIST_DB").unwrap_or_else(|_| "co_scientist.db".to_string());

    match cmd.as_str() {
        "init" => cmd_init(&db_path).await?,
        "run" => cmd_run(&args, &db_path).await?,
        "memory" => cmd_memory(&args, &db_path).await?,
        "tools" => cmd_tools(&args).await?,
        "skills" => cmd_skills(&args).await?,
        "enqueue" => cmd_enqueue(&args, &db_path).await?,
        "serve" => cmd_serve(&db_path).await?,
        "start" => cmd_start(&args, &db_path).await?,
        "promote" => cmd_promote(&db_path).await?,
        other => {
            eprintln!("unknown command: {other}");
            usage();
            std::process::exit(2);
        }
    }

    Ok(())
}

async fn cmd_init(db_path: &str) -> Result<()> {
    let d = db::open(db_path).await?;
    let mem = Memory::new(d);
    let runner = Runner::new(mem, new_run_id(), RunnerConfig::default());
    runner.seed_default_agents().await?;
    // Also create the skills directory with a tiny example skill.
    let skills_dir = std::path::PathBuf::from(
        env::var("CO_SCIENTIST_SKILLS").unwrap_or_else(|_| "co_scientist_skills".to_string()),
    );
    if !skills_dir.exists() {
        std::fs::create_dir_all(&skills_dir)?;
        let example = skills_dir.join("hello");
        std::fs::create_dir_all(example.join("scripts"))?;
        std::fs::write(
            example.join("SKILL.md"),
            "---\nname: hello\ndescription: Says hello. Takes no args.\n---\n",
        )?;
        std::fs::write(
            example.join("scripts/run.sh"),
            "#!/bin/sh\necho '{\"ok\": true, \"msg\": \"hello\"}'\n",
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = std::fs::metadata(example.join("scripts/run.sh"))?.permissions();
            p.set_mode(0o755);
            std::fs::set_permissions(example.join("scripts/run.sh"), p)?;
        }
        println!("created example skill at {}/hello", skills_dir.display());
    }
    println!("initialized {db_path} and seeded 6 agents");
    Ok(())
}

async fn cmd_tools(_args: &[String]) -> Result<()> {
    use co_scientist::registry::ToolRegistry;
    use co_scientist::tool::builtin_tools;
    let mut reg = ToolRegistry::new();
    reg.register_all(builtin_tools());
    for name in reg.names() {
        let tool = reg.get(&name).expect("just registered");
        println!("{:<20} {}", name, tool.description().lines().next().unwrap_or(""));
    }
    Ok(())
}

async fn cmd_skills(args: &[String]) -> Result<()> {
    let dir = arg_value(args, "--dir").unwrap_or_else(|| {
        env::var("CO_SCIENTIST_SKILLS").unwrap_or_else(|_| "co_scientist_skills".to_string())
    });
    let path = std::path::PathBuf::from(&dir);
    let skills = co_scientist::discover_skills(&path)?;
    if skills.is_empty() {
        println!("(no skills found in {dir}; run `co-scientist init` to scaffold an example)");
        return Ok(());
    }
    for s in &skills {
        println!(
            "{:<24} {} (entrypoint: {})",
            s.name,
            s.description.lines().next().unwrap_or(""),
            s.entrypoint.display()
        );
    }
    Ok(())
}

async fn cmd_enqueue(args: &[String], db_path: &str) -> Result<()> {
    use co_scientist::queue::{EnqueueRequest, TaskQueue};
    let action = arg_value(args, "--action").context("--action NAME is required")?;
    let session = arg_value(args, "--session").unwrap_or_else(|| new_run_id());
    let agent = arg_value(args, "--agent").unwrap_or_else(|| "experiment".to_string());
    let payload_str = positional_after_flags(args).context("missing JSON payload")?;
    let payload: serde_json::Value = serde_json::from_str(&payload_str)
        .with_context(|| format!("payload is not valid JSON: {payload_str}"))?;
    let d = db::open(db_path).await?;
    let q = TaskQueue::new(d);
    let id = q
        .enqueue(EnqueueRequest::new(session.clone(), agent, action.clone(), payload))
        .await?;
    println!("enqueued task {id} (session={session} action={action})");
    Ok(())
}

async fn cmd_serve(db_path: &str) -> Result<()> {
    use co_scientist::queue::TaskQueue;
    use co_scientist::registry::ToolRegistry;
    use co_scientist::tool::builtin_tools;
    use co_scientist::worker::{ctrl_c_shutdown_pair, run_worker, WorkerConfig};
    use std::sync::Arc;
    // Worker and consolidation each need their own DB connection.
    // (The connection returned by db::open here is intentionally unused —
    // fresh connections are opened per-component via connect_fresh below.)
    let _d = db::open(db_path).await?;
    let bus = co_scientist::EventBus::default();
    let mem = co_scientist::Memory::with_bus(
        Db::new(Db::connect_fresh(db_path).await?),
        bus.clone(),
    );
    let consolidation_mem = co_scientist::Memory::with_bus(
        Db::new(Db::connect_fresh(db_path).await?),
        bus.clone(),
    );
    let mut reg = ToolRegistry::new();
    reg.register_all(builtin_tools());
    // Try to load skills from disk too.
    let skills_dir = std::path::PathBuf::from(
        env::var("CO_SCIENTIST_SKILLS").unwrap_or_else(|_| "co_scientist_skills".to_string()),
    );
    let mut n_skills = 0;
    if skills_dir.exists() {
        for s in co_scientist::discover_skills(&skills_dir)? {
            reg.register(co_scientist::skill_to_tool(s));
            n_skills += 1;
        }
    }
    let q = TaskQueue::new(Db::new(Db::connect_fresh(db_path).await?));
    let cfg = WorkerConfig::default();
    // shutdown_tx is dropped on return — the worker and consolidation
    // tasks own clones of shutdown_rx and observe completion on their own.
    let (_shutdown_tx, shutdown_rx) = ctrl_c_shutdown_pair();

    // Spawn the background consolidation service.
    let consolidation_cfg = co_scientist::PromotionConfig::default();
    let consolidation_shutdown = shutdown_rx.clone();
    let consolidation_handle = tokio::spawn(async move {
        let svc = co_scientist::ConsolidationService::new(consolidation_mem, consolidation_cfg);
        if let Err(e) = svc.run(bus, consolidation_shutdown).await {
            tracing::error!(error = %e, "consolidation service failed");
        }
    });

    eprintln!(
        "worker {} starting (tools={}, skills={}, consolidation=active)",
        cfg.worker_id,
        reg.names().len(),
        n_skills
    );
    run_worker(mem, q, Arc::new(reg), cfg, shutdown_rx).await?;

    // Wait for consolidation service to shut down.
    let _ = consolidation_handle.await;
    Ok(())
}

async fn cmd_promote(db_path: &str) -> Result<()> {
    let d = db::open(db_path).await?;
    let mem = co_scientist::Memory::new(d);
    let cfg = co_scientist::PromotionConfig::default();
    let stats = co_scientist::promotion::run_consolidation(&mem, &cfg).await?;
    println!("consolidation complete:");
    println!("  embeddings backfilled: {}", stats.embeddings_backfilled);
    println!("  embeddings upgraded:   {}", stats.embeddings_upgraded);
    println!("  clusters found:        {}", stats.clusters_found);
    println!("  memories archived:     {}", stats.memories_archived);
    println!("  index entries added:   {}", stats.index_entries_added);
    Ok(())
}

async fn cmd_run(args: &[String], db_path: &str) -> Result<()> {
    let agent_name = arg_value(args, "--agent").context("--agent NAME is required")?;
    let user_text = positional_after_flags(args).context("missing user text")?;
    let run_id = env::var("CO_SCIENTIST_RUN_ID").unwrap_or_else(|_| new_run_id());

    let d = db::open(db_path).await?;
    let mem = Memory::new(d);
    let mut runner = Runner::new(mem, run_id.clone(), RunnerConfig::default());
    let agent = find_agent(&agent_name)?;
    let outcome = runner.turn(&agent, &user_text).await?;
    println!("{}", outcome.cleaned_text);
    if !outcome.markers.is_empty() {
        eprintln!(
            "[runner] run_id={run_id} dispatched {} memory op(s)",
            outcome.markers.len()
        );
    }
    Ok(())
}

async fn cmd_start(args: &[String], db_path: &str) -> Result<()> {
    use co_scientist::run_agent::RunAgentTool;
    use co_scientist::supervisor::{Supervisor, SupervisorConfig};
    use co_scientist::queue::TaskQueue;
    use co_scientist::registry::ToolRegistry;
    use co_scientist::tool::builtin_tools;
    use co_scientist::worker::{ctrl_c_shutdown_pair, run_worker, WorkerConfig};
    use std::sync::Arc;

    let goal = positional_after_flags(args).context("-- \"goal text\" is required")?;
    let preferences = arg_value(args, "--preferences").unwrap_or_default();
    let budget: f64 = arg_value(args, "--budget")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0);
    let deadline_secs: u64 = arg_value(args, "--deadline")
        .and_then(|s| parse_duration_secs(&s))
        .unwrap_or(0);
    let concurrency: usize = arg_value(args, "--concurrency")
        .and_then(|s| s.parse().ok())
        .unwrap_or(4);
    let n_initial: usize = arg_value(args, "--initial")
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);

    let session_id = co_scientist::memory::new_run_id();
    // The connection returned by db::open here is intentionally unused —
    // supervisor, worker, and consolidation each open their OWN fresh
    // connection below because rusqlite::Connection is single-threaded
    // and sharing one across concurrent components triggers "concurrent
    // use forbidden".
    let _d = db::open(db_path).await?;
    let bus = co_scientist::EventBus::default();
    let mem = co_scientist::Memory::with_bus(
        Db::new(Db::connect_fresh(db_path).await?),
        bus.clone(),
    );
    let worker_mem = co_scientist::Memory::with_bus(
        Db::new(Db::connect_fresh(db_path).await?),
        bus.clone(),
    );
    let consolidation_mem = co_scientist::Memory::with_bus(
        Db::new(Db::connect_fresh(db_path).await?),
        bus.clone(),
    );

    // Build registry with all tools.
    let mut reg = ToolRegistry::new();
    reg.register_all(builtin_tools());

    let skills_dir = std::path::PathBuf::from(
        env::var("CO_SCIENTIST_SKILLS").unwrap_or_else(|_| "co_scientist_skills".to_string()),
    );
    if skills_dir.exists() {
        for s in co_scientist::discover_skills(&skills_dir)? {
            reg.register(co_scientist::skill_to_tool(s));
        }
    }

    let prompts = Arc::new(co_scientist::Prompts::new()?);
    let q = TaskQueue::new(Db::new(Db::connect_fresh(db_path).await?));

    // Register the RunAgentTool (handles agent execution + follow-ups).
    let run_agent_tool = RunAgentTool::new(
        q.clone(),
        prompts.clone(),
        Arc::new(reg.clone()),
        co_scientist::runner::RunnerConfig::default(),
    );
    reg.register(Arc::new(run_agent_tool));

    let reg = Arc::new(reg);
    let (shutdown_tx, shutdown_rx) = ctrl_c_shutdown_pair();

    // Spawn the consolidation service.
    let consolidation_cfg = co_scientist::PromotionConfig::default();
    let consolidation_shutdown = shutdown_rx.clone();
    let consolidation_handle = tokio::spawn(async move {
        let svc = co_scientist::ConsolidationService::new(consolidation_mem, consolidation_cfg);
        if let Err(e) = svc.run(bus, consolidation_shutdown).await {
            tracing::error!(error = %e, "consolidation service failed");
        }
    });

    let sup_config = SupervisorConfig {
        budget_usd: budget,
        deadline: std::time::Duration::from_secs(deadline_secs),
        concurrency,
        n_initial,
        ..Default::default()
    };

    eprintln!("starting research session {}", session_id);
    eprintln!("goal: {}", goal);
    if budget > 0.0 {
        eprintln!("budget: ${:.2}", budget);
    }
    if deadline_secs > 0 {
        eprintln!("deadline: {}s", deadline_secs);
    }

    // Spawn the worker with its OWN connection.
    let worker_q = q.clone();
    let worker_reg = reg.clone();
    let worker_shutdown = shutdown_rx.clone();
    let worker_handle = tokio::spawn(async move {
        let cfg = WorkerConfig::default();
        if let Err(e) = run_worker(worker_mem, worker_q, worker_reg, cfg, worker_shutdown).await {
            tracing::error!(error = %e, "worker failed");
        }
    });

    // Run the supervisor (blocks until done).
    Supervisor::run(
        mem.clone(),
        q.clone(),
        reg.clone(),
        prompts,
        sup_config,
        session_id.clone(),
        goal,
        preferences,
        shutdown_rx.clone(),
        shutdown_tx.clone(),
    )
    .await?;

    // Wait for worker and consolidation to shut down.
    let _ = worker_handle.await;
    let _ = consolidation_handle.await;

    eprintln!("session {} complete", session_id);
    Ok(())
}

fn parse_duration_secs(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(rest) = s.strip_suffix('h') {
        rest.trim().parse::<u64>().ok().map(|h| h * 3600)
    } else if let Some(rest) = s.strip_suffix('m') {
        rest.trim().parse::<u64>().ok().map(|m| m * 60)
    } else if let Some(rest) = s.strip_suffix('s') {
        rest.trim().parse::<u64>().ok()
    } else {
        s.parse::<u64>().ok()
    }
}

async fn cmd_memory(args: &[String], db_path: &str) -> Result<()> {
    let sub = args.get(2).map(String::as_str).unwrap_or("");
    let d = db::open(db_path).await?;
    let mem = Memory::new(d);
    let conn = mem.db().conn();
    match sub {
        "count" => {
            for table in [
                "agents",
                "sessions",
                "events",
                "semantic_memories",
                "behavior_memories",
            ] {
                let mut rows = conn
                    .query(&format!("SELECT COUNT(*) FROM {table}"), ())
                    .await?;
                let n: i64 = if let Some(r) = rows.next().await? {
                    r.get(0)?
                } else {
                    0
                };
                println!("{table:20} {n}");
            }
            println!("{db_path:20} (file)");
        }
        "recent" => {
            let n: i64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(10);
            let mut rows = conn
                .query(
                    "SELECT e.id, a.name, e.step_index, e.type, e.created_at
                     FROM events e JOIN agents a ON a.id = e.agent_id
                     ORDER BY e.id DESC LIMIT ?1",
                    [n],
                )
                .await?;
            while let Some(r) = rows.next().await? {
                let id: i64 = r.get(0)?;
                let name: String = r.get(1)?;
                let step: i64 = r.get(2)?;
                let ty: String = r.get(3)?;
                let ts: String = r.get(4)?;
                println!("#{id:>5} step={step:<3} agent={name:<12} type={ty:<20} {ts}");
            }
        }
        "context" => {
            let agent = arg_value(args, "--agent").context("--agent NAME required")?;
            let query = positional_after_flags(args).context("missing query")?;
            let ctx = mem
                .get_context(
                    &new_run_id(),
                    &agent,
                    &query,
                    ContextLimits {
                        events: 5,
                        semantic: 5,
                        behavior: 3,
                        max_tokens: 0,
                        full_count: 3,
                    },
                )
                .await?;
            print!("{}", ctx.rendered);
        }
        other => {
            eprintln!("unknown memory subcommand: {other}");
            usage();
            std::process::exit(2);
        }
    }
    Ok(())
}

fn usage() {
    eprintln!("co-scientist — local agent memory layer");
    eprintln!();
    eprintln!("USAGE:");
    eprintln!("  co-scientist init");
    eprintln!("  co-scientist run --agent NAME -- \"<user text>\"");
    eprintln!("  co-scientist start -- \"<goal>\" [--preferences TEXT] [--budget USD] [--deadline 2h] [--concurrency 4] [--initial 3]");
    eprintln!("  co-scientist memory count");
    eprintln!("  co-scientist memory recent [N]");
    eprintln!("  co-scientist memory context --agent NAME -- \"<query>\"");
    eprintln!("  co-scientist tools list");
    eprintln!("  co-scientist skills list [--dir PATH]");
    eprintln!("  co-scientist enqueue --action NAME [--session ID] [--agent NAME] -- '<json>'");
    eprintln!("  co-scientist serve");
    eprintln!("  co-scientist promote");
    eprintln!();
    eprintln!("ENV:");
    eprintln!("  CO_SCIENTIST_DB      path to .db file (default: co_scientist.db)");
    eprintln!("  CO_SCIENTIST_SKILLS  path to skills dir (default: co_scientist_skills)");
    eprintln!("  CO_SCIENTIST_RUN_ID  pin a run id (default: random uuid)");
    eprintln!("  CO_SCIENTIST_MODEL   model name passed to claude (default: sonnet)");
    eprintln!("  RUST_LOG             tracing filter (default: info)");
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag && i + 1 < args.len() {
            return Some(args[i + 1].clone());
        }
        i += 1;
    }
    None
}

fn positional_after_flags(args: &[String]) -> Option<String> {
    if let Some(sep) = args.iter().position(|a| a == "--") {
        if sep + 1 < args.len() {
            return Some(args[sep + 1..].join(" "));
        }
    }
    let mut last_flag_end = 0;
    for (i, a) in args.iter().enumerate() {
        if a.starts_with("--") {
            last_flag_end = i + 2;
        }
    }
    if last_flag_end < args.len() {
        Some(args[last_flag_end..].join(" "))
    } else {
        None
    }
}

fn find_agent(name: &str) -> Result<co_scientist::Agent> {
    co_scientist::agents::AGENTS
        .iter()
        .find(|a| a.name == name)
        .cloned()
        .with_context(|| format!("unknown agent: {name}"))
}
