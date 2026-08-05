//! `flowchart` (a layered DAG, drawn top-to-bottom, merges and branches) and
//! `tree` (a strict hierarchy, one parent per node) — the two `diagram_type`s
//! whose data comes from `id`/`next` edges rather than array order. Both were
//! static clipart before this rebuild: `flowchart` always drew a fixed
//! 10-node HTTP-auth diagram no matter what the caller asked for.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::tools::viz::layout::{draw_node, size_node_capped, NodeVisual, SizedNode, GAP_X, GAP_Y, MAX_CONTENT_W, MAX_NODE_W, MIN_LAYER_GAP, MIN_NODE_H, MIN_NODE_W};
use crate::tools::viz::model::{Step, StepType};
use crate::tools::viz::render::svg::{SvgCanvas, CANVAS_MARGIN, TITLE_BAND};
use crate::tools::viz::text;

const LEFT_X: f32 = 20.0;

fn decision_badge(s: &Step) -> Option<String> {
    if s.step_type != StepType::Decision {
        return None;
    }
    s.subtitle.clone().filter(|t| !t.trim().is_empty())
}

fn resolve_edges(steps: &[Step]) -> (HashMap<&str, usize>, Vec<(usize, usize)>) {
    let id_to_index: HashMap<&str, usize> = steps.iter().enumerate().filter_map(|(i, s)| s.id.as_deref().map(|id| (id, i))).collect();
    let edges: Vec<(usize, usize)> = steps
        .iter()
        .enumerate()
        .flat_map(|(i, s)| {
            // Shadow with a `&HashMap` (a `Copy` reference) before the inner
            // closure so `move` copies the reference and `i` in on each call
            // instead of trying to move the map itself out from under the
            // outer `FnMut` closure on the first iteration.
            let id_to_index = &id_to_index;
            s.next.iter().filter_map(move |nid| id_to_index.get(nid.as_str()).map(|&j| (i, j)))
        })
        .collect();
    (id_to_index, edges)
}

/// Longest-path-from-roots layering via Kahn's algorithm. Nodes reachable
/// only through a cycle never reach in-degree zero, so a second pass appends
/// them (in original order) after every acyclic node has been layered,
/// trading strict layering for termination rather than looping forever.
fn compute_layers(n: usize, edges: &[(usize, usize)]) -> Vec<usize> {
    if n == 0 {
        return Vec::new();
    }
    let mut indegree = vec![0usize; n];
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &(u, v) in edges {
        adj[u].push(v);
        indegree[v] += 1;
    }

    let mut layer: Vec<Option<usize>> = vec![None; n];
    let mut queue: VecDeque<usize> = (0..n).filter(|&i| indegree[i] == 0).collect();
    if queue.is_empty() {
        queue.push_back(0);
    }
    for &r in &queue {
        layer[r] = Some(0);
    }

    let mut indeg_work = indegree.clone();
    let mut processed = vec![false; n];
    while let Some(u) = queue.pop_front() {
        if processed[u] {
            continue;
        }
        processed[u] = true;
        let lu = layer[u].unwrap_or(0);
        for &v in &adj[u] {
            let candidate = lu + 1;
            if layer[v].is_none_or(|lv| candidate > lv) {
                layer[v] = Some(candidate);
            }
            if indeg_work[v] > 0 {
                indeg_work[v] -= 1;
            }
            if indeg_work[v] == 0 && !processed[v] {
                queue.push_back(v);
            }
        }
    }

    let mut next_layer = layer.iter().filter_map(|l| *l).max().map(|m| m + 1).unwrap_or(0);
    for l in layer.iter_mut() {
        if l.is_none() {
            *l = Some(next_layer);
            next_layer += 1;
        }
    }
    layer.into_iter().map(|l| l.unwrap_or(0)).collect()
}

/// Orders nodes within each layer by the average original index of their
/// predecessors (a cheap barycenter proxy), which keeps chains roughly
/// straight without an iterative crossing-minimization sweep.
fn order_layers(n: usize, layer: &[usize], edges: &[(usize, usize)]) -> Vec<Vec<usize>> {
    let max_layer = layer.iter().copied().max().unwrap_or(0);
    let mut buckets: Vec<Vec<usize>> = vec![Vec::new(); max_layer + 1];
    for i in 0..n {
        buckets[layer[i]].push(i);
    }
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &(u, v) in edges {
        preds[v].push(u);
    }
    let barycenter = |ids: &[usize]| -> f32 {
        if ids.is_empty() {
            f32::MAX
        } else {
            ids.iter().sum::<usize>() as f32 / ids.len() as f32
        }
    };
    for bucket in buckets.iter_mut().skip(1) {
        bucket.sort_by(|&a, &b| barycenter(&preds[a]).partial_cmp(&barycenter(&preds[b])).unwrap_or(std::cmp::Ordering::Equal).then(a.cmp(&b)));
    }
    buckets
}

fn draw_branch_tag(canvas: &mut SvgCanvas, cx: f32, cy: f32, label: &str, dark: bool) {
    let w = (text::measure_px(label, 10.0) + 16.0).max(28.0);
    let h = 22.0;
    let (bg_class, text_class) = if dark { ("tag-bg-dark", "tag-text-light") } else { ("tag-bg-light", "tag-text-dark") };
    canvas.rect(cx - w / 2.0, cy - h / 2.0, w, h, bg_class);
    canvas.text_line(cx, cy, text_class, label);
}

pub fn render_flowchart(steps: &[Step]) -> (String, f32, f32) {
    let (id_to_index, edges) = resolve_edges(steps);
    let n = steps.len();

    let layer = compute_layers(n, &edges);
    let layers = order_layers(n, &layer, &edges);

    // Readability compression: size nodes (reducing the width cap on each
    // iteration if needed) until the widest layer fits `MAX_CONTENT_W`.
    let mut cap = MAX_NODE_W;
    let mut sized: Vec<SizedNode> = steps.iter().map(|s| size_node_capped(&s.text, decision_badge(s).as_deref(), cap)).collect();
    for _ in 0..4 {
        let widest = layers
            .iter()
            .map(|nodes| nodes.iter().map(|&i| sized[i].w).sum::<f32>() + GAP_X * (nodes.len() as f32 - 1.0).max(0.0))
            .fold(0.0_f32, f32::max);
        if widest <= MAX_CONTENT_W {
            break;
        }
        cap = (cap * (MAX_CONTENT_W - MIN_LAYER_GAP * 2.0) / widest).clamp(MIN_NODE_W, MAX_NODE_W);
        sized = steps.iter().map(|s| size_node_capped(&s.text, decision_badge(s).as_deref(), cap)).collect();
    }

    // Per-layer gap: squeeze gaps to fit the budget before shrinking nodes
    // further; `MIN_LAYER_GAP` is the floor past which nodes are shrunk instead.
    let gaps: Vec<f32> = layers
        .iter()
        .map(|nodes| {
            if nodes.len() <= 1 {
                0.0
            } else {
                let sum = nodes.iter().map(|&i| sized[i].w).sum::<f32>();
                ((MAX_CONTENT_W - sum) / (nodes.len() as f32 - 1.0)).clamp(MIN_LAYER_GAP, GAP_X)
            }
        })
        .collect();

    let mut row_h = vec![MIN_NODE_H; layers.len()];
    for (l, nodes) in layers.iter().enumerate() {
        row_h[l] = nodes.iter().map(|&i| sized[i].h).fold(MIN_NODE_H, f32::max);
    }
    let mut row_y = vec![0.0f32; layers.len()];
    let mut acc = TITLE_BAND;
    for l in 0..layers.len() {
        row_y[l] = acc;
        acc += row_h[l] + GAP_Y;
    }

    let layer_width = |nodes: &[usize], gap: f32| -> f32 {
        if nodes.is_empty() {
            0.0
        } else {
            nodes.iter().map(|&i| sized[i].w).sum::<f32>() + gap * (nodes.len() as f32 - 1.0)
        }
    };
    let widths: Vec<f32> = layers.iter().enumerate().map(|(l, nodes)| layer_width(nodes, gaps[l])).collect();
    let max_w = widths.iter().cloned().fold(0.0_f32, f32::max);

    let mut center_x = vec![0.0f32; n];
    let mut top_y = vec![0.0f32; n];
    for (l, nodes) in layers.iter().enumerate() {
        let left = LEFT_X + (max_w - widths[l]) / 2.0;
        let mut x = left;
        for &i in nodes {
            center_x[i] = x + sized[i].w / 2.0;
            top_y[i] = row_y[l] + (row_h[l] - sized[i].h) / 2.0;
            x += sized[i].w + gaps[l];
        }
    }

    let mut canvas = SvgCanvas::new();

    for &(u, v) in &edges {
        let (x1, y1) = (center_x[u], top_y[u] + sized[u].h);
        let (x2, y2) = (center_x[v], top_y[v]);
        let ymid = (y1 + y2) / 2.0;
        let d = format!("M {x1:.1},{y1:.1} C {x1:.1},{ymid:.1} {x2:.1},{ymid:.1} {x2:.1},{y2:.1}");
        let bbox = (x1.min(x2), y1.min(y2), (x1 - x2).abs(), (y2 - y1).abs());
        canvas.path(&d, "flow-path", bbox);
    }

    for (i, step) in steps.iter().enumerate() {
        if step.step_type != StepType::Decision {
            continue;
        }
        let outgoing: Vec<usize> = step.next.iter().filter_map(|nid| id_to_index.get(nid.as_str()).copied()).collect();
        if outgoing.len() < 2 {
            continue;
        }
        for (branch_idx, &target) in outgoing.iter().take(2).enumerate() {
            let (x1, y1) = (center_x[i], top_y[i] + sized[i].h);
            let (x2, y2) = (center_x[target], top_y[target]);
            // The edge curve is `M x1,y1 C x1,ymid x2,ymid x2,y2` with
            // ymid=(y1+y2)/2, so at its midpoint the curve passes through
            // exactly ((x1+x2)/2, (y1+y2)/2) -- the vertical middle of the
            // gutter. Skip-level edges are rejected in validation, so y1..y2
            // spans only this gutter and the tag can never sit on a node.
            // Clamping horizontally keeps it inside the canvas.
            let tag_x = ((x1 + x2) / 2.0).clamp(LEFT_X + 24.0, LEFT_X + max_w - 24.0);
            let tag_y = (y1 + y2) / 2.0;
            let (label, dark) = if branch_idx == 0 { ("YES", true) } else { ("NO", false) };
            draw_branch_tag(&mut canvas, tag_x, tag_y, label, dark);
        }
    }

    for (i, n) in sized.iter().enumerate() {
        draw_node(
            &mut canvas,
            center_x[i] - n.w / 2.0,
            top_y[i],
            n.w,
            n.h,
            NodeVisual { lines: &n.lines, badge: n.badge.as_deref(), step_type: steps[i].step_type },
        );
    }

    let (body, bounds) = canvas.into_parts();
    (body, bounds.width() + CANVAS_MARGIN, bounds.height() + CANVAS_MARGIN)
}

fn tree_depths(n: usize, roots: &[usize], children: &[Vec<usize>]) -> Vec<usize> {
    let mut depth = vec![0usize; n];
    let mut visited = vec![false; n];
    let mut stack: Vec<(usize, usize)> = roots.iter().map(|&r| (r, 0)).collect();
    while let Some((node, d)) = stack.pop() {
        if visited[node] {
            continue;
        }
        visited[node] = true;
        depth[node] = d;
        for &c in &children[node] {
            stack.push((c, d + 1));
        }
    }
    depth
}

fn post_order(node: usize, children: &[Vec<usize>], order: &mut Vec<usize>, visited: &mut [bool]) {
    if visited[node] {
        return;
    }
    visited[node] = true;
    for &c in &children[node] {
        post_order(c, children, order, visited);
    }
    order.push(node);
}

fn subtree_widths(n: usize, order: &[usize], children: &[Vec<usize>], sized: &[SizedNode]) -> Vec<f32> {
    let mut w = vec![0.0f32; n];
    for &i in order {
        if children[i].is_empty() {
            w[i] = sized[i].w;
        } else {
            let sum: f32 = children[i].iter().map(|&c| w[c]).sum::<f32>() + GAP_X * (children[i].len() as f32 - 1.0).max(0.0);
            w[i] = sized[i].w.max(sum);
        }
    }
    w
}

fn place_subtree(node: usize, x_start: f32, children: &[Vec<usize>], subtree_w: &[f32], center_x: &mut [f32]) {
    center_x[node] = x_start + subtree_w[node] / 2.0;
    if children[node].is_empty() {
        return;
    }
    let total_children_w: f32 = children[node].iter().map(|&c| subtree_w[c]).sum::<f32>() + GAP_X * (children[node].len() as f32 - 1.0).max(0.0);
    let mut cursor = center_x[node] - total_children_w / 2.0;
    for &c in &children[node] {
        place_subtree(c, cursor, children, subtree_w, center_x);
        cursor += subtree_w[c] + GAP_X;
    }
}

/// `type` is documented as ignored by "tree" (see the schema doc comment on
/// `AccessibleSvgRequest::diagram_type`); every node draws as a plain box
/// regardless of what `step_type` the caller sent.
pub fn render_tree(steps: &[Step]) -> (String, f32, f32) {
    let n = steps.len();
    let (id_to_index, _) = resolve_edges(steps);
    let children: Vec<Vec<usize>> =
        steps.iter().map(|s| s.next.iter().filter_map(|nid| id_to_index.get(nid.as_str()).copied()).collect()).collect();
    let referenced: HashSet<usize> = children.iter().flatten().copied().collect();
    let mut roots: Vec<usize> = (0..n).filter(|i| !referenced.contains(i)).collect();
    if roots.is_empty() {
        roots.push(0);
    }

    // Readability compression: shrink the node-width cap until the widest root
    // subtree fits `MAX_CONTENT_W` (plus the inter-root spacing).
    let mut cap = MAX_NODE_W;
    let mut sized: Vec<SizedNode> = steps.iter().map(|s| size_node_capped(&s.text, None, cap)).collect();
    for _ in 0..4 {
        let mut visited = vec![false; n];
        let mut order: Vec<usize> = Vec::with_capacity(n);
        for &r in &roots {
            post_order(r, &children, &mut order, &mut visited);
        }
        let subtree_w = subtree_widths(n, &order, &children, &sized);
        let total: f32 = roots.iter().map(|&r| subtree_w[r]).sum::<f32>() + GAP_X * 2.0 * (roots.len() as f32 - 1.0).max(0.0);
        if total <= MAX_CONTENT_W {
            break;
        }
        cap = (cap * (MAX_CONTENT_W - 40.0) / total).clamp(MIN_NODE_W, MAX_NODE_W);
        sized = steps.iter().map(|s| size_node_capped(&s.text, None, cap)).collect();
    }

    let depth = tree_depths(n, &roots, &children);
    let max_depth = depth.iter().copied().max().unwrap_or(0);
    let mut row_h = vec![MIN_NODE_H; max_depth + 1];
    for i in 0..n {
        row_h[depth[i]] = row_h[depth[i]].max(sized[i].h);
    }
    let mut row_y = vec![0.0f32; max_depth + 1];
    let mut acc = TITLE_BAND;
    for d in 0..=max_depth {
        row_y[d] = acc;
        acc += row_h[d] + GAP_Y;
    }

    let mut visited = vec![false; n];
    let mut order: Vec<usize> = Vec::with_capacity(n);
    for &r in &roots {
        post_order(r, &children, &mut order, &mut visited);
    }
    let subtree_w = subtree_widths(n, &order, &children, &sized);

    let mut center_x = vec![0.0f32; n];
    let mut cursor = LEFT_X;
    for &r in &roots {
        place_subtree(r, cursor, &children, &subtree_w, &mut center_x);
        cursor += subtree_w[r] + GAP_X * 2.0;
    }

    let mut canvas = SvgCanvas::new();
    for (i, kids) in children.iter().enumerate() {
        let y1 = row_y[depth[i]] + (row_h[depth[i]] - sized[i].h) / 2.0 + sized[i].h;
        for &c in kids {
            let (x1, x2) = (center_x[i], center_x[c]);
            let y2 = row_y[depth[c]] + (row_h[depth[c]] - sized[c].h) / 2.0;
            let mid_y = (y1 + y2) / 2.0;
            let d = format!("M {x1:.1},{y1:.1} L {x1:.1},{mid_y:.1} L {x2:.1},{mid_y:.1} L {x2:.1},{y2:.1}");
            let bbox = (x1.min(x2), y1.min(y2), (x1 - x2).abs(), (y2 - y1).abs());
            canvas.path(&d, "flow-path", bbox);
        }
    }
    for i in 0..n {
        let y = row_y[depth[i]] + (row_h[depth[i]] - sized[i].h) / 2.0;
        draw_node(
            &mut canvas,
            center_x[i] - sized[i].w / 2.0,
            y,
            sized[i].w,
            sized[i].h,
            NodeVisual { lines: &sized[i].lines, badge: None, step_type: StepType::Process },
        );
    }

    let (body, bounds) = canvas.into_parts();
    (body, bounds.width() + CANVAS_MARGIN, bounds.height() + CANVAS_MARGIN)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(id: &str, text: &str, next: &[&str]) -> Step {
        Step { id: Some(id.to_string()), text: text.to_string(), next: next.iter().map(|s| s.to_string()).collect(), ..Default::default() }
    }

    #[test]
    fn flowchart_renders_caller_data_not_the_retired_http_clipart() {
        let steps = vec![
            step("a", "Receive order", &["b"]),
            step("b", "Payment ok?", &["c", "d"]),
            step("c", "Ship it", &[]),
            step("d", "Cancel order", &[]),
        ];
        let (body, _, _) = render_flowchart(&steps);
        assert!(body.contains("Receive order"));
        assert!(body.contains("Payment ok?"));
        assert!(body.contains("Ship it"));
        assert!(body.contains("Cancel order"));
        assert!(!body.contains("Receive API Request"), "must not fall back to the retired HTTP-auth clipart");
        assert!(!body.contains("Inspect Bearer Token"), "must not fall back to the retired HTTP-auth clipart");
    }

    #[test]
    fn flowchart_merge_node_sinks_below_both_branches() {
        let steps = vec![
            step("a", "Start", &["b", "c"]),
            step("b", "Branch 1", &["d"]),
            step("c", "Branch 2", &["d"]),
            step("d", "Merge", &[]),
        ];
        let layer = compute_layers(4, &[(0, 1), (0, 2), (1, 3), (2, 3)]);
        assert_eq!(layer[3], 2, "merge node must be laid out after both of its predecessors");
        let (body, _, _) = render_flowchart(&steps);
        assert!(body.contains("Merge"));
    }

    #[test]
    fn flowchart_decision_with_two_branches_gets_yes_no_tags() {
        let steps = vec![step("a", "Check?", &["b", "c"]), step("b", "Yes path", &[]), step("c", "No path", &[])];
        let mut steps = steps;
        steps[0].step_type = StepType::Decision;
        let (body, _, _) = render_flowchart(&steps);
        assert!(body.contains(">YES<"));
        assert!(body.contains(">NO<"));
    }

    #[test]
    fn flowchart_breaks_a_cycle_instead_of_looping_forever() {
        let steps = vec![step("a", "A", &["b"]), step("b", "B", &["a"])];
        // Must terminate and produce output for both nodes.
        let (body, w, h) = render_flowchart(&steps);
        assert!(body.contains("A") && body.contains("B"));
        assert!(w > 0.0 && h > 0.0);
    }

    fn plain_step(text: &str, next: &[&str], id: &str) -> Step {
        Step { id: Some(id.to_string()), text: text.to_string(), next: next.iter().map(|s| s.to_string()).collect(), ..Default::default() }
    }

    #[test]
    fn tree_renders_hierarchy_from_caller_data() {
        let steps = vec![
            plain_step("CEO", &["vp1", "vp2"], "ceo"),
            plain_step("VP Eng", &[], "vp1"),
            plain_step("VP Sales", &[], "vp2"),
        ];
        let (body, _, _) = render_tree(&steps);
        assert!(body.contains("CEO"));
        assert!(body.contains("VP Eng"));
        assert!(body.contains("VP Sales"));
    }

    #[test]
    fn tree_children_are_centered_under_a_common_parent() {
        let steps = vec![
            plain_step("Root", &["l", "r"], "root"),
            plain_step("Left", &[], "l"),
            plain_step("Right", &[], "r"),
        ];
        let (_, w, _) = render_tree(&steps);
        assert!(w > 0.0);
    }

    #[test]
    fn every_flowchart_and_tree_node_stays_within_canvas_bounds() {
        let steps = vec![
            step("a", "Start", &["b", "c"]),
            step("b", "Left branch with a longer label", &["d"]),
            step("c", "Right", &["d"]),
            step("d", "End", &[]),
        ];
        let (body, w, h) = render_flowchart(&steps);
        for (x, y, rw, rh) in extract_rects(&body) {
            assert!(x + rw <= w + 0.5, "rect exceeds canvas width: x={x} rw={rw} w={w}");
            assert!(y + rh <= h + 0.5, "rect exceeds canvas height: y={y} rh={rh} h={h}");
        }
    }

    fn extract_rects(svg: &str) -> Vec<(f32, f32, f32, f32)> {
        let mut out = Vec::new();
        for cap_start in svg.match_indices("<rect ") {
            let tag_end = svg[cap_start.0..].find('/').map(|i| cap_start.0 + i).unwrap_or(svg.len());
            let tag = &svg[cap_start.0..tag_end];
            let get = |attr: &str| -> f32 {
                tag.split(&format!(r#"{attr}=""#)).nth(1).and_then(|rest| rest.split('"').next()).and_then(|v| v.parse().ok()).unwrap_or(0.0)
            };
            out.push((get("x"), get("y"), get("width"), get("height")));
        }
        out
    }
}
