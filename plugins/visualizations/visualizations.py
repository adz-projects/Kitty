"""
Accessible Visuals & Tables MCP Server (iframe-Ready)
------------------------------------------------------
A single-file Python FastMCP server producing WCAG 2.2 AA compliant HTML data tables
and uncrowded SVG diagrams (Flowcharts, Single-Lane Pipelines, Swimlanes, Journey Maps)
styled in an elegant, modern monochrome/grayscale palette.

All visual tools return a standardized payload with an explicit `render_config` object 
specifying `iframe` intent and a standalone HTML document ready for `srcdoc` injection.

Dependencies:
    pip install mcp

Execution:
    python accessible_viz_mcp.py
"""

import json
from typing import Any, Dict, List, Optional
from mcp.server.fastmcp import FastMCP

# Initialize FastMCP Server
mcp = FastMCP("Accessible Visuals & Tables Server")


# ---------------------------------------------------------------------------
# Standalone HTML Document Wrapper for iframe Rendering
# ---------------------------------------------------------------------------
def wrap_in_standalone_html(title: str, body_content: str) -> str:
    """Wraps raw HTML or SVG content in a full, standalone HTML document string.

    Includes an embedded ResizeObserver script that posts height updates to the parent window
    for automatic iframe height adjustments.
    """
    return f"""<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline';">
    <title>{title}</title>
    <style>
        *, *::before, *::after {{
            box-sizing: border-box;
        }}
        body {{
            margin: 0;
            padding: 1rem;
            background-color: #fafafa;
            font-family: system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
            color: #18181b;
            overflow: hidden; /* Prevents internal scrollbars; parent resizes frame */
        }}
        svg {{
            display: block;
            width: 100%;
            height: auto;
            max-width: 100%;
        }}
    </style>
</head>
<body>
    {body_content}
    <script>
        function sendHeight() {{
            const height = document.documentElement.scrollHeight;
            window.parent.postMessage({{ type: 'mcp-iframe-resize', height: height }}, '*');
        }}
        window.addEventListener('load', sendHeight);
        if (typeof ResizeObserver !== 'undefined') {{
            new ResizeObserver(sendHeight).observe(document.body);
        }}
    </script>
</body>
</html>""".strip()


# ---------------------------------------------------------------------------
# Accessible Grayscale HTML Table Builder
# ---------------------------------------------------------------------------
class GrayscaleTableBuilder:
    """Generates WCAG 2.2 AA compliant HTML tables styled in a sleek grayscale palette."""

    @staticmethod
    def render_table(
        title: str,
        headers: List[str],
        rows: List[List[Any]],
        summary: Optional[str] = None,
    ) -> str:
        """Renders an accessible HTML table fragment with explicit header scopes and row headers."""
        css = """
        <style>
            .mcp-table-wrapper {
                margin: 0;
                width: 100%;
                overflow-x: auto;
            }
            table.mcp-grayscale-table {
                width: 100%;
                border-collapse: separate;
                border-spacing: 0;
                border: 1px solid #d4d4d8;
                border-radius: 8px;
                overflow: hidden;
                font-size: 0.875rem;
                color: #18181b;
                background-color: #ffffff;
                box-shadow: 0 2px 4px rgba(0, 0, 0, 0.04);
            }
            table.mcp-grayscale-table caption {
                font-size: 1.125rem;
                font-weight: 700;
                text-align: left;
                padding: 0.75rem 0.25rem;
                color: #09090b;
                caption-side: top;
            }
            table.mcp-grayscale-table thead th {
                background-color: #18181b;
                color: #f4f4f5;
                font-weight: 600;
                text-transform: uppercase;
                font-size: 0.75rem;
                letter-spacing: 0.05em;
                padding: 0.875rem 1rem;
                border-bottom: 2px solid #09090b;
                text-align: left;
            }
            table.mcp-grayscale-table th, 
            table.mcp-grayscale-table td {
                padding: 0.75rem 1rem;
                border-bottom: 1px solid #e4e4e7;
                text-align: left;
            }
            table.mcp-grayscale-table tbody tr:nth-child(even) {
                background-color: #fafafa;
            }
            table.mcp-grayscale-table tbody tr:hover {
                background-color: #f4f4f5;
            }
            table.mcp-grayscale-table tbody tr:last-child td,
            table.mcp-grayscale-table tbody tr:last-child th {
                border-bottom: none;
            }
            table.mcp-grayscale-table th[scope="row"] {
                background-color: #f4f4f5;
                font-weight: 600;
                color: #09090b;
                border-right: 1px solid #e4e4e7;
            }
            .sr-only {
                position: absolute;
                width: 1px;
                height: 1px;
                padding: 0;
                margin: -1px;
                overflow: hidden;
                clip: rect(0, 0, 0, 0);
                white-space: nowrap;
                border: 0;
            }
        </style>
        """

        summary_html = f'<p class="sr-only">{summary}</p>' if summary else ""
        header_cells = "".join([f'<th scope="col">{h}</th>' for h in headers])

        body_rows = []
        for row in rows:
            cells = []
            for idx, val in enumerate(row):
                if idx == 0:
                    cells.append(f'<th scope="row">{val}</th>')
                else:
                    cells.append(f"<td>{val}</td>")
            body_rows.append(f"<tr>{''.join(cells)}</tr>")

        html = f"""
        <div class="mcp-table-wrapper">
            {css}
            {summary_html}
            <table class="mcp-grayscale-table">
                <caption>{title}</caption>
                <thead>
                    <tr>{header_cells}</tr>
                </thead>
                <tbody>
                    {''.join(body_rows)}
                </tbody>
            </table>
        </div>
        """
        return html.strip()


# ---------------------------------------------------------------------------
# Uncrowded Grayscale SVG Diagram Generator
# ---------------------------------------------------------------------------
class GrayscaleSVGBuilder:
    """Generates uncrowded, accessible SVG diagrams using a clean monochrome theme."""

    @staticmethod
    def get_common_defs() -> str:
        return """
        <defs>
            <marker id="arrow" viewBox="0 0 10 10" refX="6" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
                <path d="M 0 0 L 10 5 L 0 10 z" fill="#52525b"/>
            </marker>

            <filter id="soft-shadow" x="-8%" y="-8%" width="116%" height="116%">
                <feDropShadow dx="0" dy="2" stdDeviation="3" flood-color="#000000" flood-opacity="0.07"/>
            </filter>

            <style>
                .canvas-bg { fill: #fafafa; rx: 12px; }
                .title-text { font-family: system-ui, -apple-system, sans-serif; font-size: 18px; font-weight: 700; fill: #09090b; text-anchor: middle; }
                
                /* Node Styles */
                .node-box { fill: #ffffff; stroke: #27272a; stroke-width: 2px; rx: 8px; filter: url(#soft-shadow); }
                .node-box.pill { fill: #f4f4f5; stroke: #18181b; stroke-width: 2px; rx: 20px; filter: url(#soft-shadow); }
                .node-box.decision { fill: #ffffff; stroke: #09090b; stroke-width: 2.5px; rx: 8px; filter: url(#soft-shadow); }
                .node-triangle { fill: #f4f4f5; stroke: #27272a; stroke-width: 2px; stroke-linejoin: round; filter: url(#soft-shadow); }
                
                /* Typography */
                .node-text { font-family: system-ui, -apple-system, sans-serif; font-size: 12.5px; font-weight: 600; fill: #18181b; text-anchor: middle; dominant-baseline: middle; }
                .badge-meta { font-family: system-ui, -apple-system, sans-serif; font-size: 8.5px; font-weight: 800; fill: #52525b; text-anchor: middle; letter-spacing: 0.6px; }
                
                /* Vectors */
                .flow-path { stroke: #52525b; stroke-width: 2px; fill: none; marker-end: url(#arrow); stroke-linejoin: round; }
                
                /* Badges */
                .tag-bg-dark { fill: #18181b; stroke: #09090b; stroke-width: 1px; rx: 11px; }
                .tag-text-light { font-family: system-ui, -apple-system, sans-serif; font-size: 10px; font-weight: 800; fill: #f4f4f5; text-anchor: middle; dominant-baseline: middle; }
                .tag-bg-light { fill: #ffffff; stroke: #a1a1aa; stroke-width: 1.5px; rx: 11px; }
                .tag-text-dark { font-family: system-ui, -apple-system, sans-serif; font-size: 10px; font-weight: 800; fill: #27272a; text-anchor: middle; dominant-baseline: middle; }

                /* Swimlane & Journey Map Styles */
                .lane-header { font-family: system-ui, -apple-system, sans-serif; font-size: 11px; font-weight: 800; fill: #52525b; letter-spacing: 0.8px; }
                .lane-divider { stroke: #e4e4e7; stroke-width: 1px; }
                .curve-line { fill: none; stroke: #18181b; stroke-width: 3px; stroke-linecap: round; }
                .curve-dot { fill: #ffffff; stroke: #18181b; stroke-width: 3px; r: 6px; }
                .pain-card { fill: #f4f4f5; stroke: #a1a1aa; stroke-width: 1px; rx: 6px; }
                .pain-text { font-family: system-ui, -apple-system, sans-serif; font-size: 10.5px; font-weight: 600; fill: #27272a; text-anchor: middle; }
            </style>
        </defs>
        """

    @classmethod
    def render_branching_flowchart(cls, title: str, description: str) -> str:
        """Generates an uncrowded 10-node branching decision flowchart in monochrome."""
        return f"""
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 880 650" width="100%" height="auto" role="img" aria-label="{title}">
    <title>{title}</title>
    <desc>{description}</desc>
    {cls.get_common_defs()}

    <rect width="100%" height="100%" class="canvas-bg"/>
    <text x="440" y="35" class="title-text">{title}</text>

    <!-- CONNECTOR PATHS -->
    <path d="M 440,100 L 440,125" class="flow-path"/>
    <path d="M 440,170 L 440,215 A 10,10 0 0 1 430,225 L 210,225 A 10,10 0 0 0 200,235 L 200,250" class="flow-path"/>
    <path d="M 440,215 A 10,10 0 0 0 450,225 L 670,225 A 10,10 0 0 1 680,235 L 680,250" class="flow-path"/>
    <path d="M 200,290 L 200,335" class="flow-path"/>
    <path d="M 680,290 L 680,330" class="flow-path"/>
    <path d="M 680,375 L 680,415 A 10,10 0 0 1 670,425 L 550,425 A 10,10 0 0 0 540,435 L 540,450" class="flow-path"/>
    <path d="M 680,415 A 10,10 0 0 0 690,425 L 790,425 A 10,10 0 0 1 800,435 L 800,450" class="flow-path"/>
    <path d="M 540,490 L 540,530" class="flow-path"/>
    <path d="M 800,490 L 800,530" class="flow-path"/>

    <!-- BRANCH BADGES -->
    <g transform="translate(315, 225)">
        <rect x="-22" y="-11" width="44" height="22" class="tag-bg-light"/>
        <text x="0" y="1" class="tag-text-dark">NO</text>
    </g>
    <g transform="translate(565, 225)">
        <rect x="-22" y="-11" width="44" height="22" class="tag-bg-dark"/>
        <text x="0" y="1" class="tag-text-light">YES</text>
    </g>
    <g transform="translate(610, 425)">
        <rect x="-22" y="-11" width="44" height="22" class="tag-bg-light"/>
        <text x="0" y="1" class="tag-text-dark">NO</text>
    </g>
    <g transform="translate(745, 425)">
        <rect x="-22" y="-11" width="44" height="22" class="tag-bg-dark"/>
        <text x="0" y="1" class="tag-text-light">YES</text>
    </g>

    <!-- NODES -->
    <g transform="translate(340, 60)">
        <rect width="200" height="40" class="node-box pill"/>
        <text x="100" y="20" class="node-text">1. Receive API Request</text>
    </g>
    <g transform="translate(340, 125)">
        <rect width="200" height="45" class="node-box decision"/>
        <text x="100" y="12" class="badge-meta">DECISION</text>
        <text x="100" y="27" class="node-text">2. Payload Valid?</text>
    </g>
    <g transform="translate(100, 250)">
        <rect width="200" height="40" class="node-box"/>
        <text x="100" y="20" class="node-text">3. Log Request Error</text>
    </g>
    <g transform="translate(100, 335)">
        <rect width="200" height="40" class="node-box pill"/>
        <text x="100" y="20" class="node-text">4. Return 400 Bad Request</text>
    </g>
    <g transform="translate(580, 250)">
        <rect width="200" height="40" class="node-box"/>
        <text x="100" y="20" class="node-text">5. Inspect Bearer Token</text>
    </g>
    <g transform="translate(580, 330)">
        <rect width="200" height="45" class="node-box decision"/>
        <text x="100" y="12" class="badge-meta">DECISION</text>
        <text x="100" y="27" class="node-text">6. Token Active?</text>
    </g>
    <g transform="translate(450, 450)">
        <rect width="180" height="40" class="node-box"/>
        <text x="90" y="20" class="node-text">7. Trigger Challenge</text>
    </g>
    <g transform="translate(450, 530)">
        <rect width="180" height="40" class="node-box pill"/>
        <text x="90" y="20" class="node-text">8. Return 401 Error</text>
    </g>
    <g transform="translate(710, 450)">
        <rect width="180" height="40" class="node-box"/>
        <text x="90" y="20" class="node-text">9. Execute Controller</text>
    </g>
    <g transform="translate(710, 530)">
        <rect width="180" height="40" class="node-box pill"/>
        <text x="90" y="20" class="node-text">10. Return 200 OK</text>
    </g>
</svg>
""".strip()

    @classmethod
    def render_single_lane_process(cls, title: str, description: str, steps: List[Dict[str, str]]) -> str:
        """Generates a horizontal single-lane process with rectangles and widened triangle quality gates."""
        svg_width = 880
        svg_height = 220
        
        step_elements = []
        arrow_elements = []
        
        x_offset = 30
        y_center = 115
        
        for i, step in enumerate(steps):
            stype = step.get("type", "process")
            label = step.get("text", f"Step {i+1}")
            sub = step.get("subtitle", "GATE" if stype == "gate" else "")
            
            if stype == "gate":
                # Widened Triangle Node (130px wide x 70px tall)
                if len(label) > 13 and " " in label:
                    parts = label.split(" ", 1)
                    text_content = f'<text x="65" y="44" class="node-text" style="font-size:10px;"><tspan x="65" dy="0">{parts[0]}</tspan><tspan x="65" dy="12">{parts[1]}</tspan></text>'
                else:
                    text_content = f'<text x="65" y="48" class="node-text" style="font-size:11px;">{label}</text>'

                step_elements.append(f"""
                <g transform="translate({x_offset}, 80)">
                    <polygon points="65,0 130,70 0,70" class="node-triangle"/>
                    <text x="65" y="24" class="badge-meta">{sub}</text>
                    {text_content}
                </g>
                """)
                node_width = 130
            else:
                # Rectangle Process Node
                step_elements.append(f"""
                <g transform="translate({x_offset}, 90)">
                    <rect width="130" height="50" class="node-box"/>
                    <text x="65" y="25" class="node-text">{label}</text>
                </g>
                """)
                node_width = 130

            # Connector Arrow to Next Node
            if i < len(steps) - 1:
                next_x = x_offset + node_width + 40
                arrow_elements.append(f'<line x1="{x_offset + node_width}" y1="{y_center}" x2="{next_x}" y2="{y_center}" class="flow-path"/>')
                x_offset = next_x

        return f"""
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {svg_width} {svg_height}" width="100%" height="auto" role="img" aria-label="{title}">
    <title>{title}</title>
    <desc>{description}</desc>
    {cls.get_common_defs()}

    <rect width="100%" height="100%" class="canvas-bg"/>
    <text x="440" y="35" class="title-text">{title}</text>
    {''.join(arrow_elements)}
    {''.join(step_elements)}
</svg>
""".strip()

    @classmethod
    def render_swimlane_process(cls, title: str, description: str) -> str:
        """Generates a multi-actor horizontal swimlane process flow in monochrome."""
        return f"""
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 920 380" width="100%" height="auto" role="img" aria-label="{title}">
    <title>{title}</title>
    <desc>{description}</desc>
    {cls.get_common_defs()}

    <rect width="100%" height="100%" class="canvas-bg"/>

    <!-- Swimlane Rows -->
    <text x="20" y="50" class="lane-header">CUSTOMER</text>
    <line x1="0" y1="90" x2="920" y2="90" class="lane-divider"/>

    <text x="20" y="140" class="lane-header">FRONTEND APP</text>
    <line x1="0" y1="180" x2="920" y2="180" class="lane-divider"/>

    <text x="20" y="230" class="lane-header">BACKEND API</text>
    <line x1="0" y1="270" x2="920" y2="270" class="lane-divider"/>

    <text x="20" y="320" class="lane-header">DATABASE</text>

    <!-- Connector Paths -->
    <path d="M 230,50 L 230,115" class="flow-path"/>
    <path d="M 330,135 L 430,135 A 10,10 0 0 1 440,145 L 440,205" class="flow-path"/>
    <path d="M 520,225 L 620,225 A 10,10 0 0 1 630,235 L 630,295" class="flow-path"/>

    <!-- Process Nodes -->
    <g transform="translate(150, 30)">
        <rect width="160" height="40" class="node-box"/>
        <text x="80" y="20" class="node-text">1. Submit Cart</text>
    </g>
    <g transform="translate(250, 115)">
        <rect width="160" height="40" class="node-box"/>
        <text x="80" y="20" class="node-text">2. Validate Form</text>
    </g>
    <g transform="translate(360, 205)">
        <rect width="160" height="40" class="node-box"/>
        <text x="80" y="20" class="node-text">3. Process Charge</text>
    </g>
    <g transform="translate(550, 295)">
        <rect width="160" height="40" class="node-box"/>
        <text x="80" y="20" class="node-text">4. Persist Record</text>
    </g>
</svg>
""".strip()

    @classmethod
    def render_journey_map(cls, title: str, description: str) -> str:
        """Generates a user journey map with an emotion sentiment line and pain points."""
        return f"""
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 920 420" width="100%" height="auto" role="img" aria-label="{title}">
    <title>{title}</title>
    <desc>{description}</desc>
    {cls.get_common_defs()}

    <rect width="100%" height="100%" class="canvas-bg"/>

    <!-- Grid Column Highlights -->
    <rect x="170" y="10" width="170" height="400" fill="#f4f4f5" rx="4"/>
    <rect x="530" y="10" width="170" height="400" fill="#f4f4f5" rx="4"/>

    <!-- Stage Headers -->
    <text x="255" y="35" class="node-text" style="font-weight:700;">1. Discovery</text>
    <text x="435" y="35" class="node-text" style="font-weight:700;">2. Sign Up</text>
    <text x="615" y="35" class="node-text" style="font-weight:700;">3. Setup</text>
    <text x="795" y="35" class="node-text" style="font-weight:700;">4. First Success</text>

    <line x1="20" y1="50" x2="900" y2="50" stroke="#d4d4d8" stroke-width="1.5"/>

    <!-- Row 1: Actions -->
    <text x="30" y="85" class="lane-header">USER ACTION</text>
    <text x="255" y="85" class="node-text">Reads Overview</text>
    <text x="435" y="85" class="node-text">Fills Auth Form</text>
    <text x="615" y="85" class="node-text">Configures Keys</text>
    <text x="795" y="85" class="node-text">Executes Query</text>

    <line x1="20" y1="120" x2="900" y2="120" stroke="#e4e4e7"/>

    <!-- Row 2: Sentiment Curve -->
    <text x="30" y="180" class="lane-header">SENTIMENT</text>

    <path d="M 255,160 C 320,140 370,210 435,200 C 500,190 550,220 615,210 C 680,200 730,140 795,140" class="curve-line"/>

    <circle cx="255" cy="160" class="curve-dot"/>
    <circle cx="435" cy="200" class="curve-dot"/>
    <circle cx="615" cy="210" class="curve-dot"/>
    <circle cx="795" cy="140" class="curve-dot"/>

    <line x1="20" y1="260" x2="900" y2="260" stroke="#e4e4e7"/>

    <!-- Row 3: Pain Points -->
    <text x="30" y="320" class="lane-header">PAIN POINTS</text>

    <g transform="translate(365, 300)">
        <rect width="140" height="40" class="pain-card"/>
        <text x="70" y="24" class="pain-text">Too many fields</text>
    </g>
    <g transform="translate(545, 300)">
        <rect width="140" height="40" class="pain-card"/>
        <text x="70" y="24" class="pain-text">Docs hard to locate</text>
    </g>
</svg>
""".strip()


# ---------------------------------------------------------------------------
# MCP Tools Registration
# ---------------------------------------------------------------------------
@mcp.tool()
async def generate_accessible_table(
    title: str,
    headers: List[str],
    rows: List[List[Any]],
    summary: Optional[str] = None,
) -> str:
    """Generates a WCAG 2.2 AA compliant HTML table wrapped for iframe rendering.

    Args:
        title: Caption and main title of the table.
        headers: List of column header strings (e.g. ["Category", "Q1 Sales", "Q2 Sales"]).
        rows: Matrix of row values. First element in each row acts as the row header scope.
        summary: Optional screen-reader description summarizing key dataset trends.

    Returns:
        JSON object containing status, render_config (target='iframe'), and standalone html_payload.
    """
    raw_table_fragment = GrayscaleTableBuilder.render_table(title, headers, rows, summary)
    standalone_doc = wrap_in_standalone_html(title, raw_table_fragment)

    payload = {
        "status": "success",
        "render_config": {
            "target": "iframe",
            "title": title,
            "sandbox": "allow-scripts",  # Required for postMessage auto-height script
        },
        "html_payload": standalone_doc,
    }
    return json.dumps(payload, indent=2)


@mcp.tool()
async def generate_accessible_svg(
    diagram_type: str,
    title: str,
    description: str,
    steps: Optional[List[Dict[str, str]]] = None,
) -> str:
    """Generates uncrowded, WCAG 2.2 AA compliant SVG diagrams wrapped for iframe rendering.

    Args:
        diagram_type: One of 'flowchart', 'single_lane', 'swimlane', or 'journey_map'.
        title: Visual title rendered at the top of the SVG.
        description: Screen-reader text explanation injected into <desc> for accessibility.
        steps: Optional list of step dicts for single_lane diagrams (e.g., [{"text": "Ingest", "type": "process"}, {"text": "Valid?", "type": "gate"}]).

    Returns:
        JSON object containing status, render_config (target='iframe'), and standalone html_payload.
    """
    dtype = diagram_type.lower().strip()
    
    if dtype == "flowchart":
        svg_raw = GrayscaleSVGBuilder.render_branching_flowchart(title, description)
    elif dtype == "single_lane":
        default_steps = steps or [
            {"text": "1. Ingest Data", "type": "process"},
            {"text": "2. Schema Valid?", "type": "gate", "subtitle": "GATE"},
            {"text": "3. Transform", "type": "process"},
            {"text": "4. Security Audit?", "type": "gate", "subtitle": "AUDIT"},
            {"text": "5. Publish Event", "type": "process"},
        ]
        svg_raw = GrayscaleSVGBuilder.render_single_lane_process(title, description, default_steps)
    elif dtype == "swimlane":
        svg_raw = GrayscaleSVGBuilder.render_swimlane_process(title, description)
    elif dtype == "journey_map":
        svg_raw = GrayscaleSVGBuilder.render_journey_map(title, description)
    else:
        return json.dumps({
            "status": "error", 
            "message": f"Unsupported diagram_type '{diagram_type}'. Choose from 'flowchart', 'single_lane', 'swimlane', or 'journey_map'."
        })

    standalone_doc = wrap_in_standalone_html(title, svg_raw)

    payload = {
        "status": "success",
        "render_config": {
            "target": "iframe",
            "title": title,
            "sandbox": "allow-scripts",  # Required for postMessage auto-height script
        },
        "html_payload": standalone_doc,
    }
    return json.dumps(payload, indent=2)


def main() -> None:
    mcp.run(transport="stdio")


if __name__ == "__main__":
    main()