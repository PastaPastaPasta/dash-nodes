// Pages without their own script: just the shared header and footer.
import { chrome } from "./common.js";

chrome(document.body.dataset.page || "");
