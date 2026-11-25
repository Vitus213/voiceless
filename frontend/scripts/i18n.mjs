// Keeps the interface's English and the index of its settings in step
// with the source:
//
//   src/strings.en.json      every string and template shape handed to t()
//                            in src/, each with its English
//   src/settings-index.json  every settings row: its label, help, page and
//                            section, for the settings search
//
//   node scripts/i18n.mjs --check   fail on a string without English, a stale
//                                    entry, a moved setting or Chinese outside
//                                    t() (part of `npm run build`)
//   node scripts/i18n.mjs --write   add the strings that are new (left for
//                                    translation) and drop the stale ones
import { readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import ts from 'typescript';

const src = resolve(dirname(fileURLToPath(import.meta.url)), '../src');
const STRINGS = join(src, 'strings.en.json');
const INDEX = join(src, 'settings-index.json');
const CJK = /\p{Script=Han}/u;
const functionsIn = (node) => {
  const found = [];
  const visit = (child) => {
    if (ts.isFunctionDeclaration(child) || ts.isArrowFunction(child) || ts.isFunctionExpression(child)) found.push(child);
    ts.forEachChild(child, visit);
  };
  visit(node);
  return found;
};

/** The string a `t()` argument stands for: a literal as it is, a template as its shape with `{}` per expression. */
function shapeOf(node) {
  if (ts.isStringLiteral(node) || ts.isNoSubstitutionTemplateLiteral(node)) return node.text;
  if (!ts.isTemplateExpression(node)) return null;
  return node.templateSpans.reduce((shape, span) => `${shape}{}${span.literal.text}`, node.head.text);
}

/** The literal behind a `t("…")` or a bare `"…"`; null for anything else. */
function literalOf(node) {
  if (!node) return null;
  const inner = ts.isCallExpression(node) && node.expression.getText() === 't' ? node.arguments[0] : node;
  return ts.isStringLiteral(inner) || ts.isNoSubstitutionTemplateLiteral(inner) ? inner.text : null;
}

/**
 * Names of the functions that build a settings row from their first
 * parameter (`function binding(title) { … return row(title, …) }`): a call
 * to one of them is a settings row too.
 */
function rowHelpersOf(parsed) {
  const helpers = new Set(['row']);
  for (const fn of functionsIn(parsed)) {
    const first = fn.parameters[0]?.name;
    const label = first && ts.isIdentifier(first) ? first.text : null;
    if (!label) continue;
    let builds = false;
    const visit = (node) => {
      if (ts.isCallExpression(node) && node.expression.getText(parsed) === 'row'
        && ts.isIdentifier(node.arguments[0]) && node.arguments[0].text === label) builds = true;
      ts.forEachChild(node, visit);
    };
    if (fn.body) visit(fn.body);
    if (!builds) continue;
    if (fn.name && ts.isIdentifier(fn.name)) helpers.add(fn.name.text);
    else if (ts.isVariableDeclaration(fn.parent) && ts.isIdentifier(fn.parent.name)) helpers.add(fn.parent.name.text);
  }
  return helpers;
}

/** `page` and `title` of each page factory, from the `routes` array of main.ts. */
function routesOf(main) {
  const routes = {};
  const visit = (node) => {
    if (ts.isVariableDeclaration(node) && node.name.getText(main) === 'routes' && node.initializer && ts.isArrayLiteralExpression(node.initializer)) {
      for (const element of node.initializer.elements) {
        if (!ts.isObjectLiteralExpression(element)) continue;
        const field = (name) => element.properties.find((property) => property.name?.getText(main) === name)?.initializer;
        routes[field('create')?.getText(main)] = { page: field('id').text, title: literalOf(field('title')) };
      }
    }
    ts.forEachChild(node, visit);
  };
  visit(main);
  return routes;
}

// main.ts first: its routes tell which page builds a settings row.
const files = readdirSync(src).filter((name) => /\.(js|ts)$/u.test(name) && name !== 'i18n.ts')
  .sort((a, b) => Number(b === 'main.ts') - Number(a === 'main.ts') || a.localeCompare(b));
const strings = new Map();
const patterns = new Map();
const settings = [];
const unwrapped = [];
let routes = {};

for (const name of files) {
  const file = join(src, name);
  const parsed = ts.createSourceFile(file, readFileSync(file, 'utf8'), ts.ScriptTarget.Latest, true);
  if (name === 'main.ts') routes = routesOf(parsed);
  const rowLike = rowHelpersOf(parsed);
  const visit = (node) => {
    // What t() is given: a literal or a template shape as it is, and every
    // Chinese literal inside any other argument, which is looked up at run time.
    if (ts.isCallExpression(node) && node.expression.getText(parsed) === 't') {
      const shape = shapeOf(node.arguments[0]);
      if (shape !== null && CJK.test(shape)) (ts.isTemplateExpression(node.arguments[0]) ? patterns : strings).set(shape, undefined);
      const collect = (child) => {
        if ((ts.isStringLiteral(child) || ts.isNoSubstitutionTemplateLiteral(child)) && CJK.test(child.text)) strings.set(child.text, undefined);
        ts.forEachChild(child, collect);
      };
      collect(node.arguments[0]);
    }
    // A settings row with a plain label: row(label, control, { tip }).
    if (ts.isCallExpression(node) && rowLike.has(node.expression.getText(parsed)) && literalOf(node.arguments[0]) !== null) {
      const label = literalOf(node.arguments[0]);
      const options = node.arguments[2];
      const tip = options && ts.isObjectLiteralExpression(options)
        ? options.properties.find((property) => property.name?.getText(parsed) === 'tip')?.initializer
        : undefined;
      let section = null;
      for (let ancestor = node.parent; ancestor; ancestor = ancestor.parent) {
        if (ts.isCallExpression(ancestor) && ancestor.expression.getText(parsed) === 'group') section = literalOf(ancestor.arguments[1]);
      }
      // The page factory is the nearest enclosing function that mounts a route.
      for (let ancestor = node.parent; ancestor; ancestor = ancestor.parent) {
        if (!ts.isFunctionDeclaration(ancestor) || !ancestor.name) continue;
        const route = routes[ancestor.name.text];
        if (!route) continue;
        settings.push({ page: route.page, title: route.title, section, label, ...(literalOf(tip) ? { tip: literalOf(tip) } : {}) });
        break;
      }
    }
    // Chinese that is not wrapped: it would stay Chinese in the English interface.
    const literal = ts.isStringLiteral(node) || ts.isNoSubstitutionTemplateLiteral(node) ? node
      : ts.isTemplateExpression(node) ? node : null;
    if (literal && CJK.test(literal.text ?? literal.head.text) && node.parent?.name !== node) {
      // Wrapped: an enclosing call is t(). A language names itself: `/* i18n-literal */ "中文"`.
      const source = parsed.text;
      const start = node.getStart(parsed);
      const selfNamed = /\/\*\s*i18n-literal\s*\*\/\s*$/u.test(source.slice(Math.max(0, start - 60), start));
      let wrapped = selfNamed;
      for (let ancestor = node.parent; ancestor && !wrapped; ancestor = ancestor.parent) {
        wrapped = ts.isCallExpression(ancestor) && ancestor.expression.getText(parsed) === 't';
      }
      if (!wrapped) unwrapped.push(`${name}:${parsed.getLineAndCharacterOfPosition(start).line + 1} ${node.getText(parsed)}`);
    }
    ts.forEachChild(node, visit);
  };
  visit(parsed);
}

const current = JSON.parse(readFileSync(STRINGS, 'utf8'));
const english = {
  strings: Object.fromEntries([...strings].map(([shape]) => [shape, current.strings[shape] ?? ''])),
  patterns: Object.fromEntries([...patterns].map(([shape]) => [shape, current.patterns[shape] ?? ''])),
};
const nextIndex = JSON.stringify(settings);

if (process.argv.includes('--write')) {
  writeFileSync(STRINGS, `${JSON.stringify(english, null, 2)}\n`);
  writeFileSync(INDEX, `${nextIndex}\n`);
  console.log(`strings.en.json: ${strings.size} strings, ${patterns.size} shapes, ${Object.values(english).flatMap((table) => Object.values(table)).filter((value) => !value).length} without English`);
  console.log(`settings-index.json: ${settings.length} settings`);
} else {
  const missing = Object.entries(english).flatMap(([table, entries]) => Object.entries(entries).filter(([, value]) => !value).map(([shape]) => `${table}: ${shape}`));
  const stale = ['strings', 'patterns'].filter((table) => JSON.stringify(Object.keys(current[table] ?? {})) !== JSON.stringify(Object.keys(english[table])))
    .map((table) => `${table}: the keys of strings.en.json differ from the source; run node scripts/i18n.mjs --write`);
  const problems = [
    ...missing.map((entry) => `without English  ${entry}`),
    ...stale,
    ...(readFileSync(INDEX, 'utf8').replace(/\r\n/g, '\n') === `${nextIndex}\n` ? [] : ['settings-index.json is out of date; run node scripts/i18n.mjs --write']),
    ...unwrapped.map((entry) => `Chinese outside t()  ${entry}`),
  ];
  if (problems.length) {
    console.error(problems.join('\n'));
    process.exit(1);
  }
  console.log(`i18n: ${strings.size} strings and ${patterns.size} shapes have English; ${settings.length} settings indexed`);
}
