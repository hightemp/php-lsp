<?php

declare(strict_types=1);

namespace App\Lsp;

// CASE: Same-scope variable references are supported by textDocument/references.
function variableReferencesSupported(int $seed): int
{
    $value = $seed + 1;
    $total = $value + 2;
    return $total + $value;
}
