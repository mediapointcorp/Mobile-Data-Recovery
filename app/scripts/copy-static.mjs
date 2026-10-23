// Copies static renderer assets next to the compiled renderer script.
import { copyFileSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const out = join(root, "dist", "renderer", "renderer");
mkdirSync(out, { recursive: true });
for (const f of ["index.html", "styles.css"]) {
  copyFileSync(join(root, "src", "renderer", f), join(out, f));
}
