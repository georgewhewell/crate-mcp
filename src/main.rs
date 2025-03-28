use std::collections::HashMap;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use cargo::GlobalContext;
use cargo::core::{PackageSet, Resolve, Workspace};
use cargo::util::StableHasher;
use mcp_sdk::server::Server;
use mcp_sdk::transport::ServerStdioTransport;
use mcp_sdk::types::{
    CallToolRequest, CallToolResponse, ListRequest, ResourcesListResponse, ServerCapabilities,
    ToolResponseContent, ToolsListResponse,
};
use regex::Regex;
use rustdoc_types::Id;
use serde_json::json;
use tracing_subscriber::EnvFilter;

use std::hash::Hash;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_env_filter(EnvFilter::from_default_env())
        // needs to be stderr due to stdio transport
        .with_writer(std::io::stderr)
        .init();

    let server = Server::builder(ServerStdioTransport)
        .capabilities(ServerCapabilities {
            tools: Some(json!({})),
            ..Default::default()
        })
        .request_handler("tools/list", list_tools)
        .request_handler("tools/call", |req: CallToolRequest| {
            let name = req.name.clone();
            call_tool(req).map_err(|e| anyhow::anyhow!("Error calling tool {1}: {:#}", e, name))
        })
        .request_handler("resources/list", |_req: ListRequest| {
            Ok(ResourcesListResponse {
                resources: vec![],
                next_cursor: None,
                meta: None,
            })
        })
        .build();
    let server_handle = {
        let server = server;
        tokio::spawn(async move { server.listen().await })
    };

    server_handle
        .await?
        .map_err(|e| anyhow::anyhow!("Server error: {}", e))?;
    Ok(())
}

fn call_tool(req: CallToolRequest) -> Result<CallToolResponse> {
    let name = req.name.as_str();
    let args = req.arguments.unwrap_or_default();
    let result = match name {
        "list_crates" => {
            let manifest_path_str = args["manifest_path"].as_str().unwrap_or("./Cargo.toml");
            let manifest_path = PathBuf::from(manifest_path_str);
            let lock_path = manifest_path
                .parent()
                .unwrap_or(Path::new("."))
                .join("Cargo.lock");

            let lockfile = cargo_lock::Lockfile::load(&lock_path)
                .with_context(|| format!("Failed to load lockfile at {lock_path:?}"))?;

            let mut crate_list = String::new();
            // Sort packages for deterministic output
            let mut packages: Vec<_> = lockfile.packages.iter().collect();
            packages.sort_unstable_by_key(|p| p.name.as_str());

            for package in packages {
                crate_list.push_str(&format!("{} v{}\n", package.name.as_str(), package.version));
            }
            ToolResponseContent::Text { text: crate_list }
        }
        "crate_public_api" => {
            let gctx = cargo::util::context::GlobalContext::default()?;
            // Optionally configure verbosity, color etc. if needed based on req or defaults
            // gctx.configure(...)?;

            let crate_name = args["crate_name"]
                .as_str()
                .context("Missing required argument 'crate_name'")?;
            let manifest_path_str = args["manifest_path"].as_str().unwrap_or("./Cargo.toml");
            let manifest_path = PathBuf::from(manifest_path_str)
                .canonicalize()
                .context("Canonicalizing manifest path")?;
            let item_filter_pattern = args["item_filter"].as_str();
            let include_doc_comments = args["include_doc_comments"].as_bool().unwrap_or(true);

            let api_text = generate_crate_public_api(
                &gctx, // Pass gctx
                crate_name,
                &manifest_path,
                item_filter_pattern,
                include_doc_comments,
            )?;
            ToolResponseContent::Text { text: api_text }
        }
        _ => return Err(anyhow!("Unsupported tool name: {name}")),
    };
    Ok(CallToolResponse {
        content: vec![result],
        is_error: None,
        meta: None,
    })
}

fn find_crate_src_path(
    gctx: &GlobalContext,
    crate_name: &str,
    host_manifest_path: &Path,
) -> Result<PathBuf> {
    // 2. Create a Workspace for the host project
    let ws = Workspace::new(&host_manifest_path, gctx) // Pass gctx here
        .with_context(|| format!("Failed to load workspace for {host_manifest_path:?}"))?;

    // ... (rest of the function remains the same) ...
    tracing::info!("Resolving workspace dependencies to find '{crate_name}'...");
    let (package_set, resolve): (PackageSet<'_>, Resolve) =
        cargo::ops::resolve_ws(&ws, false /* dry_run */)
            .with_context(|| "Failed to resolve dependencies for the workspace")?;
    tracing::info!("Dependency resolution complete.");

    let target_package_id = resolve.iter()
        .find(|pkg_id| pkg_id.name().as_str() == crate_name)
        .with_context(|| format!("Crate '{crate_name}' not found in the resolved dependency graph for workspace {host_manifest_path:?}"))?;

    tracing::debug!("Found PackageId: {}", target_package_id);

    let package = package_set.get_one(target_package_id).with_context(|| {
        format!("Failed to get package details for {target_package_id} from PackageSet")
    })?;

    let root_path = package.root().to_path_buf();

    tracing::info!(
        "Found source path for '{} {}' at: {:?}",
        crate_name,
        package.version(),
        root_path
    );

    if !root_path.exists() {
        return Err(anyhow!(
            "Source path for {} does not exist: {:?}. Try running `cargo fetch` manually first.",
            crate_name,
            root_path
        ));
    }

    Ok(root_path)
}

fn generate_crate_public_api(
    gctx: &GlobalContext, // Accept GlobalContext
    crate_name: &str,
    host_manifest_path: &Path, // Manifest of the project *using* the dependency
    item_filter_pattern: Option<&str>,
    include_doc_comments: bool,
) -> Result<String> {
    // 1. Find the dependency's source path
    //    Pass gctx down.
    let crate_src_path = find_crate_src_path(gctx, crate_name, host_manifest_path)
        .with_context(|| format!("Failed to find source path for crate '{crate_name}'"))?;
    let dep_manifest_path = crate_src_path.join("Cargo.toml");

    // Get PackageId to generate stable hash for target dir
    let ws = Workspace::new(host_manifest_path, gctx)?; // Need workspace again to get PackageId easily
    let (_package_set, resolve) = cargo::ops::resolve_ws(&ws, false)?;
    let target_package_id = resolve
        .iter()
        .find(|pkg_id| pkg_id.name().as_str() == crate_name)
        .context("Crate not found after resolving (should not happen here)")?; // Should exist from find_crate_src_path

    // 2. Ensure necessary nightly toolchain is installed (remains the same)
    let min_nightly = public_api::MINIMUM_NIGHTLY_RUST_VERSION;
    rustup_toolchain::install(min_nightly).with_context(|| {
        format!("Failed to install required nightly toolchain '{min_nightly}' using rustup")
    })?;

    // 3. Determine and create the target directory within the workspace target
    // Calculate a stable hash for the package ID to create a unique subdir name
    let mut hasher = StableHasher::new();
    target_package_id.stable_hash(ws.root()).hash(&mut hasher);
    let pkg_hash = std::hash::Hasher::finish(&hasher);

    // Get the workspace's target directory
    let workspace_target_dir = ws.target_dir();
    let mut build_target_dir = workspace_target_dir.join("mcp_rustdoc_json");
    build_target_dir.push(format!("{}-{:016x}", target_package_id.name(), pkg_hash)); // Unique subdir

    // Ensure the directory exists

    std::fs::create_dir_all(build_target_dir.as_path_unlocked())
        .with_context(|| format!("Failed to create build target directory {build_target_dir:?}"))?;

    tracing::info!(
        "Using build target directory: {:?}",
        build_target_dir.as_path_unlocked()
    );

    // 4. Build rustdoc JSON using rustdoc_json crate
    //    Use the calculated build_target_dir instead of tempdir
    let json_path = rustdoc_json::Builder::default()
        .toolchain(min_nightly)
        .manifest_path(&dep_manifest_path)
        .target_dir(build_target_dir.as_path_unlocked()) // Use the calculated path
        .build()
        .with_context(|| {
            format!("Failed to build rustdoc JSON for {crate_name} at {dep_manifest_path:?}")
        })?;

    // 5. Parse the JSON file (remains the same)
    let json_file = std::fs::File::open(&json_path)
        .with_context(|| format!("Failed to open rustdoc JSON file at {json_path:?}"))?;
    let reader = BufReader::new(json_file);
    let rustdoc_crate: rustdoc_types::Crate = serde_json::from_reader(reader)
        .with_context(|| format!("Failed to parse rustdoc JSON file at {json_path:?}"))?;

    // 6. Use public_api crate (remains the same)
    let public_api_analysis = public_api::Builder::from_rustdoc_json(&json_path)
        .omit_blanket_impls(false)
        .omit_auto_trait_impls(false)
        .omit_auto_derived_impls(false)
        .sorted(true)
        .build()
        .context("Failed to analyze public API from rustdoc JSON")?;

    // 7. Create a lookup map (remains the same)
    let item_map: HashMap<Id, &rustdoc_types::Item> = rustdoc_crate
        .index
        .iter()
        .map(|(k, v)| (*k, v)) // Dereference Id
        .collect();

    // 8. Compile regex filter (remains the same)
    let regex = match item_filter_pattern {
        Some(pattern) => {
            Some(Regex::new(pattern).with_context(|| format!("Invalid regex pattern: {pattern}"))?)
        }
        None => None,
    };

    // 9. Format the output (remains the same)
    let mut output = String::new();
    for public_item in public_api_analysis.items() {
        let item_string = public_item.to_string();

        // Apply filtering
        if let Some(re) = regex.as_ref() {
            if !re.is_match(&item_string) {
                continue;
            }
        }

        output.push_str(&item_string);
        output.push('\n');

        // Add doc comments if requested and available
        if include_doc_comments {
            if let Some(original_item) = item_map.get(&public_item.id()) {
                if let Some(docs) = &original_item.docs {
                    for line in docs.lines() {
                        output.push_str("  /// ");
                        output.push_str(line);
                        output.push('\n');
                    }
                    if !docs.is_empty() {
                        output.push('\n');
                    }
                }
            }
        }
    }

    // No explicit cleanup of build_target_dir needed, let Cargo manage it.

    Ok(output)
}

fn list_tools(_req: ListRequest) -> Result<ToolsListResponse> {
    let response = json!({
      "tools": [
     {  "name": "list_crates",
        "description":
          "Lists all dependencies (package name and version) found in the Cargo.lock file relative to the specified manifest path (defaults to ./Cargo.toml).",
        "inputSchema": {
          "type": "object",
          "properties": {
            "manifest_path": {
              "type": "string",
              "description": "Path to the Cargo.toml file. Defaults to './Cargo.toml'. The Cargo.lock in the same directory will be used.",
              "nullable": true
            }
          }
        }
      },
      {
        "name": "crate_public_api",
        "description":
          "Generates the public API listing for a specific crate dependency found in the Cargo registry cache. Requires the crate version to be present in the project's Cargo.lock.",
        "inputSchema": {
          "type": "object",
          "properties": {
            "crate_name": {
              "type": "string",
              "description": "The name of the crate dependency (as used in Cargo.toml or Cargo.lock)."
            },
            "manifest_path": {
                "type": "string",
                "description": "Path to the Cargo.toml file of the *host* project (used to find Cargo.lock and crate version). Defaults to './Cargo.toml'.",
                "nullable": true
            },
            "item_filter": {
              "type": "string",
              "description": "Optional regex pattern to filter the output items. Only items whose full string representation matches the pattern will be included.",
              "nullable": true
            },
            "include_doc_comments": {
              "type": "boolean",
              "description": "Whether to include documentation comments in the output. Defaults to true.",
              "nullable": true,
              "default": true
            }
          },
          "required": ["crate_name"]
        }
      },
      ],
    });
    Ok(serde_json::from_value(response)?)
}
