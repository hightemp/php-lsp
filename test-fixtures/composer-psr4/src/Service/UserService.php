<?php

declare(strict_types=1);

namespace App\Service;

use App\Entity\User;

class UserService
{
    /** @var User[] */
    private array $users = [];

    public function addUser(User $user): void
    {
        $this->users[] = $user;
    }

    public function findByName(string $name): ?User
    {
        foreach ($this->users as $user) {
            if ($user->getName() === $name) {
                return $user;
            }
        }
        return null;
    }

    public function count(): int
    {
        return count($this->users);
    }
}
