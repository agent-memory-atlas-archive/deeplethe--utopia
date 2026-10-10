import type { BusinessRule, RelationTypeView } from "../api";

export type RuleExpression =
  | { attr: string }
  | { const: number | string }
  | { text: string }
  | { op: "add" | "sub" | "mul" | "div"; l: RuleExpression; r: RuleExpression }
  | { cast: "number"; expr: RuleExpression }
  | { case: RuleExpression; when: { is: number | string; then: number | string }[]; else?: number | string }
  | { date_trunc: "year" | "month" | "day"; expr: RuleExpression };

const scalar = (value: unknown): value is number | string =>
  typeof value === "string" || (typeof value === "number" && Number.isFinite(value));

// Match the store's root-at-zero validation bound. Unknown nodes stay unknown;
// a reader must never replace them with a constant or simplify their structure.
export function readExpression(raw: unknown, depth = 0): RuleExpression | null {
  if (depth > 4 || !raw || typeof raw !== "object" || Array.isArray(raw)) return null;
  const node = raw as Record<string, unknown>;
  if (Object.keys(node).length === 1 && typeof node.attr === "string" &&
      /^[0-9a-f]{8}(-[0-9a-f]{4}){3}-[0-9a-f]{12}$/i.test(node.attr)) {
    return { attr: node.attr };
  }
  if (Object.keys(node).length === 1 &&
      (typeof node.const === "number" || typeof node.const === "string") &&
      /^[+-]?(?:\d+(?:\.\d*)?|\.\d+)(?:[eE][+-]?\d+)?$/.test(String(node.const).trim()) && Number.isFinite(Number(node.const))) {
    return { const: node.const };
  }
  if (Object.keys(node).length === 1 && typeof node.text === "string") return { text: node.text };
  if (Object.keys(node).length === 2 && node.cast === "number") {
    const expr = readExpression(node.expr, depth + 1);
    return expr ? { cast: "number", expr } : null;
  }
  if (Object.keys(node).length === 2 && ["year", "month", "day"].includes(String(node.date_trunc))) {
    const expr = readExpression(node.expr, depth + 1);
    return expr ? { date_trunc: node.date_trunc as "year" | "month" | "day", expr } : null;
  }
  if ("case" in node) {
    const hasElse = Object.hasOwn(node, "else");
    if (Object.keys(node).length !== 2 + Number(hasElse) || !Array.isArray(node.when) ||
        !node.when.length || node.when.length > 32 || (hasElse && !scalar(node.else))) return null;
    const expr = readExpression(node.case, depth + 1);
    if (!expr) return null;
    const arms: { is: number | string; then: number | string }[] = [];
    for (const raw of node.when) {
      if (!raw || typeof raw !== "object" || Array.isArray(raw) || Object.keys(raw).length !== 2 ||
          !scalar(raw.is) || !scalar(raw.then)) return null;
      arms.push({ is: raw.is, then: raw.then });
    }
    if (arms.some((a) => typeof a.then !== typeof arms[0].then) ||
        (hasElse && typeof node.else !== typeof arms[0].then)) return null;
    return { case: expr, when: arms, ...(hasElse ? { else: node.else as number | string } : {}) };
  }
  if (Object.keys(node).length !== 3 || !["add", "sub", "mul", "div"].includes(String(node.op))) return null;
  const l = readExpression(node.l, depth + 1), r = readExpression(node.r, depth + 1);
  return l && r ? { op: node.op as "add" | "sub" | "mul" | "div", l, r } : null;
}

export function expressionText(raw: unknown, attributes: Pick<RelationTypeView, "id" | "label" | "key">[], unknownText: string): string {
  const expr = readExpression(raw);
  if (!expr) return unknownText;
  const labels = new Map(attributes.map((a) => [a.id, a]));
  const counts = new Map<string, number>();
  for (const a of attributes) counts.set(a.label, (counts.get(a.label) ?? 0) + 1);
  // SQL keywords are expression notation, not UI copy. Quoting keeps a literal
  // label distinguishable from an attribute or an operator in either language.
  const literal = (value: number | string) => typeof value === "number" ? String(value) : `'${value.replaceAll("'", "''")}'`;
  const render = (node: RuleExpression): string => {
    if ("attr" in node) {
      const attr = labels.get(node.attr);
      return attr ? (counts.get(attr.label)! > 1 ? `${attr.label} (${attr.key})` : attr.label) : node.attr;
    }
    if ("const" in node) return String(node.const);
    if ("text" in node) return literal(node.text);
    if ("cast" in node) return `CAST(${render(node.expr)} AS DOUBLE PRECISION)`;
    if ("date_trunc" in node) return `DATE_TRUNC('${node.date_trunc}', ${render(node.expr)})`;
    if ("case" in node) return `CASE ${render(node.case)} ${node.when.map((arm) => `WHEN ${literal(arm.is)} THEN ${literal(arm.then)}`).join(" ")}${node.else === undefined ? "" : ` ELSE ${literal(node.else)}`} END`;
    return `(${render(node.l)} ${{ add: "+", sub: "−", mul: "×", div: "÷" }[node.op]} ${render(node.r)})`;
  };
  return render(expr);
}

/** The constant form cannot round-trip these definitions. It may edit their
 * name and description through PATCH, leaving every semantic field on the server. */
export function metadataOnly(rule: BusinessRule): boolean {
  if (!["typing", "attribute"].includes(rule.conclusion)) return true;
  if (rule.conclusion === "attribute" && typeof rule.conclude_value !== "string") return true;
  return rule.conditions.some((c) => {
    if (["gt", "gte", "lt", "lte"].includes(c.op)) {
      return !["number", "string"].includes(typeof c.operand);
    }
    if (["in", "not_in"].includes(c.op)) {
      return !Array.isArray(c.operand) || c.operand.some((v) => typeof v !== "string" || v.trim() !== v || !v || v.includes(","));
    }
    if (c.op === "between") return !Array.isArray(c.operand) || c.operand.length !== 2 || c.operand.some((v) => typeof v !== "number");
    return c.op !== "present" || c.operand != null;
  });
}

export function metadataPatch(name: string, description: string) {
  return { name, description };
}
