// One-off migration: wraps every Chinese string literal and template in
// the given source files in t(...), the interface's translation call, and
// adds the import. Comments and object keys are left alone; keys are
// reported so they can be handled by hand.
//
//   node scripts/i18n-codemod.mjs src/console.js src/setup.ts src/main.ts
import { readFileSync, writeFileSync } from 'node:fs';
import ts from 'typescript';

const CJK = /\p{Script=Han}/u;

/** Whether a template expression encloses `node` (its literals are wrapped with it). */
function insideTemplate(node) {
  for (let ancestor = node.parent; ancestor; ancestor = ancestor.parent) {
    if (ts.isTemplateSpan(ancestor)) return true;
  }
  return false;
}

/** The literal `node` stands for when it is, or contains, Chinese; null otherwise. */
function chinese(node, parsed) {
  if (ts.isStringLiteral(node) || ts.isNoSubstitutionTemplateLiteral(node)) return CJK.test(node.text) ? node : null;
  // A template counts as Chinese when any of its text is, inside its expressions included.
  return ts.isTemplateExpression(node) && CJK.test(node.getText(parsed)) ? node : null;
}

/** Index just past the template literal that starts at the backtick `from`. */
function templateEnd(text, from) {
  let index = from + 1;
  while (index < text.length) {
    if (text[index] === '\\') index += 2;
    else if (text[index] === '`') return index + 1;
    else if (text[index] === '$' && text[index + 1] === '{') index = expressionEnd(text, index + 2);
    else index += 1;
  }
  return text.length;
}

/** Index just past the `}` that closes an expression that starts at `from`. */
function expressionEnd(text, from) {
  let depth = 1;
  let index = from;
  while (index < text.length && depth > 0) {
    const char = text[index];
    if (char === '{') depth += 1;
    else if (char === '}') depth -= 1;
    else if (char === '`') index = templateEnd(text, index) - 1;
    else if (char === '"' || char === "'") {
      index += 1;
      while (index < text.length && text[index] !== char) index += text[index] === '\\' ? 2 : 1;
    }
    index += 1;
  }
  return index;
}

/**
 * `text`, a template literal, with every Chinese string or template
 * literal inside its `${}` expressions wrapped in t(). The parser's
 * positions for nodes inside a template are not used: they are read off
 * the text itself.
 */
function wrapInside(text) {
  const out = [];
  let index = 0;
  while (index < text.length) {
    const char = text[index];
    if (char === '`' || char === '\\') {
      out.push(text.slice(index, index + (char === '\\' ? 2 : 1)));
      index += char === '\\' ? 2 : 1;
    } else if (char === '$' && text[index + 1] === '{') {
      const end = expressionEnd(text, index + 2);
      const expression = text.slice(index, end);
      out.push(CJK.test(expression) ? `\${t(${expression.slice(2, -1)})}` : expression);
      index = end;
    } else {
      out.push(char);
      index += 1;
    }
  }
  return out.join('');
}

for (const file of process.argv.slice(2)) {
  const source = readFileSync(file, 'utf8');
  const parsed = ts.createSourceFile(file, source, ts.ScriptTarget.Latest, true);
  const edits = [];
  const keys = [];
  const visit = (node) => {
    const literal = chinese(node, parsed);
    if (literal) {
      const parent = node.parent;
      const isKey = parent.name === node && (ts.isPropertyAssignment(parent) || ts.isPropertySignature(parent) || ts.isEnumMember(parent));
      const done = ts.isCallExpression(parent) && parent.expression.getText(parsed) === 't';
      if (isKey) keys.push(node.text);
      // A literal inside a template is wrapped with the template.
      else if (!done && !insideTemplate(node)) {
        const text = literal.getText(parsed);
        if (!ts.isTemplateExpression(literal)) edits.push([literal.getStart(parsed), literal.getEnd(), `t(${text})`]);
        else {
          const wrapped = wrapInside(text);
          // Chinese in its own words gets the whole template translated; Chinese only inside an expression gets the expression.
          const ownWords = CJK.test(literal.head.text + literal.templateSpans.map((span) => span.literal.text).join(''));
          edits.push([literal.getStart(parsed), literal.getEnd(), ownWords ? `t(${wrapped})` : wrapped]);
        }
      }
    }
    ts.forEachChild(node, visit);
  };
  visit(parsed);

  let output = source;
  for (const [start, end, replacement] of edits.sort((a, b) => b[0] - a[0])) output = output.slice(0, start) + replacement + output.slice(end);
  if (edits.length && !/^import \{ t \} from '\.\/i18n';$/mu.test(output)) {
    const firstImport = output.search(/^import /mu);
    output = firstImport < 0 ? `import { t } from './i18n';\n${output}` : `${output.slice(0, firstImport)}import { t } from './i18n';\n${output.slice(firstImport)}`;
  }
  writeFileSync(file, output);
  console.log(`${file}: ${edits.length} strings wrapped, ${keys.length} keys left: ${JSON.stringify(keys)}`);
}
