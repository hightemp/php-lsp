<?php

declare(strict_types=1);

namespace App\Lsp;

// CASE: Variable references are currently unsupported by textDocument/references.
function variableReferencesNotSupported(int $seed): int
{
    $value = $seed + 1;
    $total = $value + 2;
    return $total + $value;
}

