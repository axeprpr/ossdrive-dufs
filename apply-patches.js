import fs from 'node:fs';

const file = 'node_modules/subsrt/lib/subsrt.js';
let source = fs.readFileSync(file, 'utf8');
const marker = 'var subsrt = {';
if (!source.includes("const vttHandler = require('./format/vtt.js');")) {
    source = source.replace(marker, "const vttHandler = require('./format/vtt.js');\n\n" + marker);
    source = source.replace('format: { }', 'format: { vtt: vttHandler }');
    source = source.replace(/\(function init\(\) \{[\s\S]*?\n\}\)\(\);/, '');
    fs.writeFileSync(file, source);
}
