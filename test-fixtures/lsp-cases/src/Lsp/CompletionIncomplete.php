<?php

declare(strict_types=1);

namespace App\Lsp;

use App\Model\User;
use App\Service\UserService;

class CompletionIncomplete
{
    public function run(UserService $service, string $userName): void
    {
        // CASE: Variable completion after "$" should suggest "$userName" and "$this".
        $

        // CASE: Member completion after "$this->" should suggest current class members.
        $this->

        // CASE: Static completion after "User::" should suggest static members/constants.
        User::

        // CASE: Namespace completion after "App\" should suggest matching indexed symbols.
        App\

        // CASE: Free completion with prefix "ret" should suggest keyword "return".
        ret
    }
}

// NOTE: This file is intentionally syntactically incomplete for completion testing.
// EXPECTED: parser should produce syntax diagnostics while completion still works near cursor.

