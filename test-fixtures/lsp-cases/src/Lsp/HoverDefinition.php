<?php

declare(strict_types=1);

namespace App\Lsp;

use App\Model\User;
use App\Service\UserService;

// CASE: Hover on class/method/property and go-to-definition across files.
class HoverDefinition
{
    private UserService $service;

    public function __construct()
    {
        $this->service = new UserService();
    }

    public function demo(string $name): string
    {
        // CASE: Go-to-definition on UserService and create().
        $user = $this->service->create($name);

        // CASE: Hover/go-to-definition on User and getName().
        return $user->getName();
    }
}

// CASE: Free function symbol for hover and definition.
function makeDefaultUser(): User
{
    return User::fromName('fixture');
}

